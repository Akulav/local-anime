//! Episode downloads: a persistent queue that downloads one episode at a time, and playback
//! of downloaded episodes from disk.
//!
//! Every download is a row in the `downloads` table. Its `source` keeps the stream as it was
//! when the download started (media playlist + request headers), so the episode still plays
//! from disk after the provider's signed links expire. Saved downloads (the Download button)
//! stay until deleted; pre-downloads and watched episodes expire after the cache age.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde::Serialize;

use crate::resolver::{PlayableSource, Quality, Subtitle};
use crate::store::{now_secs, DownloadRecord};
use crate::stream::{JobProgress, SavedSource, SavedSub};
use crate::Dango;

/// No progress for this long (while nothing is playing) restarts the download once with
/// fresh links, then gives up.
const STALL: Duration = Duration::from_secs(180);

#[derive(Default)]
pub(crate) struct Queue {
    /// Record keys; saved downloads come before pre-downloads.
    pending: Mutex<VecDeque<(String, bool)>>,
    active: Mutex<Option<String>>,
    wake: tokio::sync::Notify,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadView {
    #[serde(flatten)]
    pub record: DownloadRecord,
    /// "queued", "downloading", "done", "paused" or "failed".
    pub state: &'static str,
    pub playing: bool,
    /// Unix seconds when a download that isn't saved gets deleted.
    pub expires_at: Option<i64>,
}

enum Watch {
    Done,
    Stopped,
    Stalled,
}

pub fn download_key(id: i64, episode: &str, mode: &str) -> String {
    crate::stream::sha_key(&format!("{id}:{episode}:{mode}"))
}

impl Dango {
    fn mode(&self) -> String {
        self.settings.read().mode.clone()
    }

    fn max_age_secs(&self) -> i64 {
        self.settings.read().cache_max_age_hours as i64 * 3600
    }

    fn new_record(
        &self,
        id: i64,
        episode: &str,
        provider: &str,
        title: &str,
        cover: Option<&str>,
        pinned: bool,
    ) -> DownloadRecord {
        let mode = self.mode();
        DownloadRecord {
            key: download_key(id, episode, &mode),
            anilist_id: id,
            episode: episode.to_string(),
            mode,
            title: title.to_string(),
            cover: cover.map(|c| self.original_image(c)),
            provider: provider.to_string(),
            pinned,
            source: None,
            total: 0,
            done: 0,
            bytes: 0,
            error: None,
            created_at: 0,
            used_at: 0,
        }
    }

    /// Queues episodes for download. Saved (`pinned`) downloads go ahead of pre-downloads and
    /// turn an existing pre-download into a saved one; pre-downloads never touch existing rows.
    pub fn download(
        self: &Arc<Self>,
        id: i64,
        provider: &str,
        episodes: &[String],
        title: &str,
        cover: Option<&str>,
        pinned: bool,
    ) -> Result<()> {
        for ep in episodes {
            let rec = self.new_record(id, ep, provider, title, cover, pinned);
            let existing = self.store.download(&rec.key);
            if !pinned && existing.is_some() {
                continue;
            }
            self.store.download_add(&rec)?;
            if !existing.is_some_and(|r| r.total > 0 && r.done >= r.total) {
                self.enqueue(&rec.key, pinned);
            }
        }
        Ok(())
    }

    fn enqueue(&self, key: &str, pinned: bool) {
        let q = &self.downloads;
        if q.active.lock().as_deref() == Some(key) {
            return;
        }
        let mut p = q.pending.lock();
        p.retain(|(k, _)| k != key);
        let at = if pinned { p.iter().position(|(_, pin)| !pin).unwrap_or(p.len()) } else { p.len() };
        p.insert(at, (key.to_string(), pinned));
        drop(p);
        q.wake.notify_one();
    }

    /// Remembers what the player is streaming, so the episode shows up in Downloads and plays
    /// from disk next time. The player already downloads it; this only records the source.
    #[allow(clippy::too_many_arguments)]
    pub fn track_playback(
        self: &Arc<Self>,
        id: i64,
        provider: String,
        episode: String,
        url: String,
        subtitles: Vec<Subtitle>,
        title: String,
        cover: Option<String>,
    ) {
        let rec = self.new_record(id, &episode, &provider, &title, cover.as_deref(), false);
        if self.stream.saved(&rec.key).is_some() {
            self.store.download_touch(&rec.key);
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move {
            let res = async {
                let src = me.stream.prepare(&url).await?;
                me.store.download_add(&rec)?;
                me.save_source(&rec.key, &provider, src, &subtitles).await
            }
            .await;
            if let Err(e) = res {
                tracing::debug!("not tracking playback of {title} EP {episode}: {e:#}");
            }
        });
    }

    async fn save_source(
        &self,
        key: &str,
        provider: &str,
        mut src: SavedSource,
        subtitles: &[Subtitle],
    ) -> Result<Arc<SavedSource>> {
        for s in subtitles.iter().take(8) {
            if let Ok(vtt) = self.stream.fetch_vtt(&s.url).await {
                src.subtitles.push(SavedSub { label: s.label.clone(), lang: s.lang.clone(), vtt });
            }
        }
        let src = Arc::new(src);
        self.store.download_set_source(key, provider, &serde_json::to_string(&*src)?)?;
        self.stream.put_saved(key, src.clone());
        Ok(src)
    }

    /// Loads saved sources, then works through the queue forever.
    pub(crate) fn spawn_downloader(self: &Arc<Self>) {
        let me = self.clone();
        self.rt.spawn(async move {
            for r in me.store.downloads().unwrap_or_default() {
                if let Some(src) = r.source.as_deref().and_then(|s| serde_json::from_str::<SavedSource>(s).ok()) {
                    me.stream.put_saved(&r.key, Arc::new(src));
                }
            }
            me.refresh_disk_progress().await;
            me.resume_downloads();
            loop {
                let next = me.downloads.pending.lock().pop_front();
                let Some((key, _)) = next else {
                    me.downloads.wake.notified().await;
                    continue;
                };
                *me.downloads.active.lock() = Some(key.clone());
                if let Err(e) = me.run_download(&key).await {
                    tracing::info!("download failed: {e:#}");
                    me.store.download_error(&key, &format!("{e:#}"));
                }
                *me.downloads.active.lock() = None;
            }
        });
    }

    /// Queues every unfinished download that is saved or not yet expired.
    pub(crate) fn resume_downloads(&self) {
        let cutoff = now_secs() - self.max_age_secs();
        for r in self.store.downloads().unwrap_or_default() {
            let complete = r.total > 0 && r.done >= r.total;
            if !complete && r.error.is_none() && (r.pinned || r.used_at > cutoff) {
                self.enqueue(&r.key, r.pinned);
            }
        }
    }

    async fn run_download(self: &Arc<Self>, key: &str) -> Result<()> {
        let mut refresh = false;
        loop {
            let Some(rec) = self.store.download(key) else { return Ok(()) };
            let src = match self.stream.saved(key) {
                Some(s) if !refresh => s,
                _ => self.resolve_download(&rec, refresh).await?,
            };
            let stream = self.stream.clone();
            let src2 = src.clone();
            let disk = tokio::task::spawn_blocking(move || stream.disk_progress(&src2)).await?;
            self.store.download_progress(key, disk.total, disk.done, disk.bytes);
            if disk.complete() {
                return Ok(());
            }
            self.stream.start_saved(&src);
            match self.watch_download(key, &src).await {
                Watch::Done | Watch::Stopped => return Ok(()),
                Watch::Stalled if !refresh => refresh = true,
                Watch::Stalled => bail!("download stalled, the source stopped responding"),
            }
        }
    }

    async fn resolve_download(self: &Arc<Self>, rec: &DownloadRecord, refresh: bool) -> Result<Arc<SavedSource>> {
        if rec.mode != self.mode() {
            bail!("switch to {} to download this episode", rec.mode.to_uppercase());
        }
        let mut sources = self.streams(rec.anilist_id, &rec.provider, &rec.episode, refresh).await?;
        // Same choice the player makes: native sources first, first quality.
        sources.sort_by_key(|s| s.kind == "iframe");
        let src = sources
            .into_iter()
            .find(|s| s.kind != "iframe")
            .ok_or_else(|| anyhow!("{} only offers embedded players", rec.provider))?;
        let url = src.qualities.first().map(|q| q.url.clone()).ok_or_else(|| anyhow!("no stream"))?;
        let saved = self.stream.prepare(&url).await?;
        self.save_source(&rec.key, &rec.provider, saved, &src.subtitles).await
    }

    async fn watch_download(&self, key: &str, src: &SavedSource) -> Watch {
        let mut last = (u64::MAX, Instant::now());
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if self.stream.saved(key).is_none() {
                return Watch::Stopped;
            }
            let Some(p) = self.stream.live_progress(src) else { return Watch::Stopped };
            self.store.download_progress(key, p.total, p.done, p.bytes);
            if p.complete() || !p.running {
                return Watch::Done;
            }
            // Background downloads pause while the player fetches another episode.
            if p.done != last.0 || self.stream.player_active() {
                last = (p.done, Instant::now());
            } else if last.1.elapsed() > STALL {
                return Watch::Stalled;
            }
        }
    }

    /// Every download with its live progress, oldest first.
    pub fn downloads(&self) -> Result<Vec<DownloadView>> {
        let max_age = self.max_age_secs();
        let pending: HashSet<String> = self.downloads.pending.lock().iter().map(|(k, _)| k.clone()).collect();
        let active = self.downloads.active.lock().clone();
        let mut out = Vec::new();
        for mut r in self.store.downloads()? {
            let live: Option<JobProgress> = self.stream.saved(&r.key).and_then(|s| self.stream.live_progress(&s));
            if let Some(p) = live {
                (r.total, r.done, r.bytes) = (p.total, p.done, p.bytes);
            }
            let state = if r.total > 0 && r.done >= r.total {
                "done"
            } else if active.as_deref() == Some(r.key.as_str()) || live.is_some_and(|p| p.running) {
                "downloading"
            } else if pending.contains(&r.key) {
                "queued"
            } else if r.error.is_some() {
                "failed"
            } else {
                "paused"
            };
            let expires_at = (!r.pinned).then_some(r.used_at + max_age);
            r.cover = r.cover.as_deref().map(|c| self.localize_url(c));
            out.push(DownloadView { playing: live.is_some_and(|p| p.playing), state, expires_at, record: r });
        }
        Ok(out)
    }

    /// The downloaded copy of an episode, if anything of it is on disk.
    pub fn local_source(&self, id: i64, episode: &str) -> Option<PlayableSource> {
        let key = download_key(id, episode, &self.mode());
        let rec = self.store.download(&key)?;
        let src = self.stream.saved(&key)?;
        let done = self.stream.live_progress(&src).map(|p| p.done).unwrap_or(rec.done);
        if done == 0 {
            return None;
        }
        self.store.download_touch(&key);
        Some(PlayableSource {
            provider: rec.provider,
            name: "Downloaded".into(),
            kind: if src.kind == "hls" { "hls" } else { "mp4" }.into(),
            qualities: vec![Quality { label: "Downloaded".into(), url: self.stream.saved_url(&key, &src) }],
            subtitles: src
                .subtitles
                .iter()
                .enumerate()
                .map(|(i, s)| Subtitle {
                    label: s.label.clone(),
                    lang: s.lang.clone(),
                    url: self.stream.saved_sub_url(&key, i),
                })
                .collect(),
        })
    }

    pub async fn delete_download(&self, key: &str) -> Result<()> {
        self.downloads.pending.lock().retain(|(k, _)| k != key);
        let paths = self.stream.saved(key).map(|s| self.stream.saved_paths(&s)).unwrap_or_default();
        self.stream.remove_saved(key);
        self.store.download_delete(key)?;
        tokio::task::spawn_blocking(move || {
            for p in paths {
                let _ = std::fs::remove_file(p);
            }
        })
        .await?;
        Ok(())
    }

    /// Saves a download (kept until deleted) or turns it back into one that expires.
    pub fn pin_download(&self, key: &str, pinned: bool) -> Result<()> {
        self.store.download_pin(key, pinned)?;
        if pinned {
            self.resume_download(key);
        }
        Ok(())
    }

    pub fn resume_download(&self, key: &str) {
        if let Some(r) = self.store.download(key) {
            if r.total == 0 || r.done < r.total {
                // Re-adding clears the error.
                let _ = self.store.download_add(&r);
                self.enqueue(key, r.pinned);
            }
        }
    }

    /// Pauses everything: clears the queue and cancels running downloads.
    pub fn stop_downloads(&self) {
        self.downloads.pending.lock().clear();
        self.stream.stop_downloads();
    }

    /// Deletes downloads that aren't saved once they are older than the cache age.
    pub(crate) async fn expire_downloads(&self) {
        let cutoff = now_secs() - self.max_age_secs();
        let active = self.downloads.active.lock().clone();
        for r in self.store.downloads().unwrap_or_default() {
            if !r.pinned && r.used_at < cutoff && active.as_deref() != Some(r.key.as_str()) {
                let _ = self.delete_download(&r.key).await;
            }
        }
    }

    /// Files the janitor must not touch: saved downloads and whatever is downloading now.
    pub(crate) fn protected_files(&self) -> HashSet<PathBuf> {
        let mut keep: HashSet<PathBuf> = self.stream.busy_files().into_iter().collect();
        let active = self.downloads.active.lock().clone();
        for r in self.store.downloads().unwrap_or_default() {
            if r.pinned || active.as_deref() == Some(r.key.as_str()) {
                if let Some(s) = self.stream.saved(&r.key) {
                    keep.extend(self.stream.saved_paths(&s));
                }
            }
        }
        keep
    }

    /// Re-counts what is on disk for downloads that aren't running (the janitor may have
    /// removed parts of them).
    pub(crate) async fn refresh_disk_progress(&self) {
        let items: Vec<(String, Arc<SavedSource>)> = self
            .store
            .downloads()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| self.stream.saved(&r.key).map(|s| (r.key, s)))
            .filter(|(_, s)| self.stream.live_progress(s).is_none())
            .collect();
        let stream = self.stream.clone();
        let counts = tokio::task::spawn_blocking(move || {
            items.into_iter().map(|(k, s)| (k, stream.disk_progress(&s))).collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for (k, p) in counts {
            self.store.download_progress(&k, p.total, p.done, p.bytes);
        }
    }
}
