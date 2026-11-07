//! Episode downloads: a persistent queue that downloads one episode at a time, and playback
//! of downloaded episodes from disk.
//!
//! Every download is a row in the `downloads` table. Its `source` keeps the stream as it was
//! when the download started (media playlist + request headers), so the episode still plays
//! from disk after the provider's signed links expire.
//!
//! Saved downloads (the Download button, `pinned`) are permanent: once complete, their files move
//! from the cache into the downloads folder (`Paths::downloads`), which cache cleanup and "Clear
//! cache" never touch. Only deleting the download removes them. A saved download that fails is
//! retried automatically (other working providers too), with growing pauses, and again at every
//! start. Pre-downloads and watched episodes stay in the cache and expire after the cache age.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde::Serialize;

use crate::resolver::{PlayableSource, Quality, SourceState, Subtitle};
use crate::store::{now_secs, DownloadRecord};
use crate::stream::{JobProgress, SavedSource, SavedSub};
use crate::Tsukimi;

/// No progress for this long (while nothing is playing) restarts the download once with
/// fresh links, then counts as a failure.
const STALL: Duration = Duration::from_secs(180);
/// Pauses before the automatic retries of a failed saved download.
const RETRY_AFTER: [u64; 6] = [60, 3 * 60, 10 * 60, 30 * 60, 60 * 60, 3 * 60 * 60];

#[derive(Default)]
pub(crate) struct Queue {
    /// Record keys; saved downloads come before pre-downloads.
    pending: Mutex<VecDeque<(String, bool)>>,
    active: Mutex<Option<String>>,
    wake: tokio::sync::Notify,
    /// Failed saved downloads waiting for their automatic retry: (attempts so far, when).
    retry: Mutex<HashMap<String, (usize, Instant)>>,
    /// Bumped by "Pause all", so a running download can tell a pause from a failure.
    generation: AtomicUsize,
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
    /// Unix seconds of the next automatic retry of a failed saved download.
    pub retry_at: Option<i64>,
}

enum Watch {
    Done,
    Stopped,
    Stalled,
}

pub fn download_key(id: i64, episode: &str, mode: &str) -> String {
    crate::stream::sha_key(&format!("{id}:{episode}:{mode}"))
}

impl Tsukimi {
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
            self.downloads.retry.lock().remove(&rec.key);
            match existing {
                // already complete: only make it permanent
                Some(r) if r.total > 0 && r.done >= r.total => self.keep_in_background(&rec.key),
                _ => self.enqueue(&rec.key, pinned),
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
            // Saved downloads finished before downloads were kept apart (dango, older versions)
            // move to the downloads folder now.
            for r in me.store.downloads().unwrap_or_default().into_iter().filter(|r| r.pinned) {
                if r.total > 0 && r.done >= r.total {
                    me.keep(&r.key, true).await;
                }
            }
            me.resume_downloads(true);
            loop {
                let next = me.downloads.pending.lock().pop_front();
                let Some((key, _)) = next else {
                    me.downloads.wake.notified().await;
                    continue;
                };
                *me.downloads.active.lock() = Some(key.clone());
                match me.run_download(&key).await {
                    Ok(()) => {
                        me.downloads.retry.lock().remove(&key);
                    }
                    Err(e) => me.download_failed(&key, &e),
                }
                *me.downloads.active.lock() = None;
            }
        });
        // Automatic retries of failed saved downloads.
        let me = self.clone();
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20)).await;
                let due: Vec<String> = me
                    .downloads
                    .retry
                    .lock()
                    .iter()
                    .filter(|(_, (_, at))| *at <= Instant::now())
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in due {
                    if let Some((n, at)) = me.downloads.retry.lock().get_mut(&key) {
                        // (stays in the map with its attempt count; pushed far out until it fails again)
                        *at = Instant::now() + Duration::from_secs(365 * 86_400);
                        tracing::info!("retrying download {key} (attempt {})", *n + 1);
                    }
                    me.enqueue(&key, true);
                }
            }
        });
    }

    fn download_failed(&self, key: &str, e: &anyhow::Error) {
        tracing::info!("download failed: {e:#}");
        self.store.download_error(key, &format!("{e:#}"));
        let pinned = self.store.download(key).is_some_and(|r| r.pinned);
        if !pinned {
            return;
        }
        let mut retry = self.downloads.retry.lock();
        let attempts = retry.get(key).map(|(n, _)| *n).unwrap_or(0);
        match RETRY_AFTER.get(attempts) {
            Some(&secs) => {
                retry.insert(key.to_string(), (attempts + 1, Instant::now() + Duration::from_secs(secs)));
            }
            None => {
                // out of automatic retries: waits for Retry or the next start
                retry.remove(key);
            }
        }
    }

    /// Queues every unfinished download that is saved or not yet expired. `with_failed` also
    /// retries saved downloads that failed before (at start).
    pub(crate) fn resume_downloads(&self, with_failed: bool) {
        let cutoff = now_secs() - self.max_age_secs();
        for r in self.store.downloads().unwrap_or_default() {
            let complete = r.total > 0 && r.done >= r.total;
            let retry = r.error.is_none() || (with_failed && r.pinned);
            if !complete && retry && (r.pinned || r.used_at > cutoff) {
                self.enqueue(&r.key, r.pinned);
            }
        }
    }

    async fn run_download(self: &Arc<Self>, key: &str) -> Result<()> {
        let generation = self.downloads.generation.load(Ordering::Relaxed);
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
                return self.finished(key).await;
            }
            self.stream.start_saved(&src);
            match self.watch_download(key, &src).await {
                Watch::Done => return self.finished(key).await,
                // paused by the user, or deleted
                Watch::Stopped
                    if self.downloads.generation.load(Ordering::Relaxed) != generation
                        || self.stream.saved(key).is_none() =>
                {
                    return Ok(())
                }
                // the transfer itself gave up (a progressive file the source cut off)
                Watch::Stopped => return self.finished(key).await,
                Watch::Stalled if !refresh => refresh = true,
                Watch::Stalled => bail!("download stalled, the source stopped responding"),
            }
        }
    }

    /// A download ran to its end: saved ones move to the downloads folder. A download that
    /// stopped with parts missing (the source failed for some) is an error, so it is retried.
    async fn finished(&self, key: &str) -> Result<()> {
        let (Some(rec), Some(src)) = (self.store.download(key), self.stream.saved(key)) else { return Ok(()) };
        let stream = self.stream.clone();
        let disk = tokio::task::spawn_blocking(move || stream.disk_progress(&src)).await?;
        self.store.download_progress(key, disk.total, disk.done, disk.bytes);
        if !disk.complete() {
            bail!("{} of {} parts downloaded, the source failed for the rest", disk.done, disk.total.max(1));
        }
        self.store.download_clear_error(key);
        if rec.pinned {
            self.keep(key, true).await;
        }
        Ok(())
    }

    /// Moves a download's files into the downloads folder (`true`) or back into the cache.
    async fn keep(&self, key: &str, keep: bool) {
        let Some(src) = self.stream.saved(key) else { return };
        let stream = self.stream.clone();
        let moved = tokio::task::spawn_blocking(move || stream.keep_files(&src, keep)).await.unwrap_or(0);
        if moved > 0 {
            tracing::info!("{} {moved} files of download {key}", if keep { "kept" } else { "released" });
        }
    }

    fn keep_in_background(self: &Arc<Self>, key: &str) {
        let (me, key) = (self.clone(), key.to_string());
        self.rt.spawn(async move { me.keep(&key, true).await });
    }

    /// The providers to try for a download: its own first, then (same SUB/DUB mode only) every
    /// other working one in rank order.
    async fn download_providers(self: &Arc<Self>, rec: &DownloadRecord) -> Vec<String> {
        let mut out = vec![rec.provider.clone()];
        if rec.mode != self.mode() {
            return out;
        }
        let Ok(titles) = self.titles(rec.anilist_id).await else { return out };
        let deadline = Instant::now() + Duration::from_secs(60);
        let snap = loop {
            let snap = self.resolver.probe(titles.clone(), false);
            if snap.done || Instant::now() > deadline {
                break snap;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        for s in snap.sources {
            if s.state == SourceState::Ok && s.playable != Some(false) && !s.embed_only && !out.contains(&s.provider) {
                out.push(s.provider);
            }
        }
        out
    }

    async fn resolve_download(self: &Arc<Self>, rec: &DownloadRecord, refresh: bool) -> Result<Arc<SavedSource>> {
        let mut errors = Vec::new();
        for (i, provider) in self.download_providers(rec).await.into_iter().enumerate() {
            match self.resolve_with(rec, &provider, refresh || i > 0).await {
                Ok(src) => {
                    if provider != rec.provider {
                        tracing::info!(
                            "{} EP {}: {} failed, downloading from {provider}",
                            rec.title,
                            rec.episode,
                            rec.provider
                        );
                    }
                    return Ok(src);
                }
                Err(e) => errors.push(format!("{provider}: {e:#}")),
            }
            if errors.len() >= 4 {
                break;
            }
        }
        bail!("{}", errors.join(" · "))
    }

    async fn resolve_with(
        self: &Arc<Self>,
        rec: &DownloadRecord,
        provider: &str,
        refresh: bool,
    ) -> Result<Arc<SavedSource>> {
        let mut sources = self.streams_in(rec.anilist_id, provider, &rec.episode, &rec.mode, refresh).await?;
        // Same choice the player makes: native sources first, first quality.
        sources.sort_by_key(|s| s.kind == "iframe");
        let mut last = anyhow!("{provider} only offers embedded players");
        for src in sources.into_iter().filter(|s| s.kind != "iframe") {
            let Some(url) = src.qualities.first().map(|q| q.url.clone()) else { continue };
            match self.stream.prepare(&url).await {
                Ok(saved) => return self.save_source(&rec.key, provider, saved, &src.subtitles).await,
                Err(e) => last = e,
            }
        }
        Err(last)
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
        let retry = self.downloads.retry.lock().clone();
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
            let retry_at = retry
                .get(&r.key)
                .filter(|_| state == "failed")
                .map(|(_, at)| now_secs() + at.saturating_duration_since(Instant::now()).as_secs().min(86_400) as i64);
            let expires_at = (!r.pinned).then_some(r.used_at + max_age);
            r.cover = r.cover.as_deref().map(|c| self.localize_url(c));
            out.push(DownloadView { playing: live.is_some_and(|p| p.playing), state, expires_at, retry_at, record: r });
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

    /// Deletes a download and its files (from the downloads folder and the cache). Files that
    /// another saved download shares (segments are stored by URL) stay.
    pub async fn delete_download(&self, key: &str) -> Result<()> {
        self.downloads.pending.lock().retain(|(k, _)| k != key);
        self.downloads.retry.lock().remove(key);
        let mut paths = self.stream.saved(key).map(|s| self.stream.all_saved_paths(&s)).unwrap_or_default();
        self.stream.remove_saved(key);
        self.store.download_delete(key)?;
        if !paths.is_empty() {
            let shared: HashSet<PathBuf> = self
                .store
                .downloads()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.pinned)
                .filter_map(|r| self.stream.saved(&r.key))
                .flat_map(|s| self.stream.all_saved_paths(&s))
                .collect();
            paths.retain(|p| !shared.contains(p));
        }
        tokio::task::spawn_blocking(move || {
            for p in paths {
                let _ = std::fs::remove_file(p);
            }
        })
        .await?;
        Ok(())
    }

    /// Saves a download (kept until deleted) or turns it back into one that expires.
    pub async fn pin_download(self: &Arc<Self>, key: &str, pinned: bool) -> Result<()> {
        self.store.download_pin(key, pinned)?;
        if pinned {
            self.resume_download(key);
            self.keep(key, true).await;
        } else {
            self.downloads.retry.lock().remove(key);
            self.keep(key, false).await;
        }
        Ok(())
    }

    /// Queues a download again (also a failed one: its automatic retries start over).
    pub fn resume_download(&self, key: &str) {
        if let Some(r) = self.store.download(key) {
            self.downloads.retry.lock().remove(key);
            if r.total == 0 || r.done < r.total {
                // Re-adding clears the error.
                let _ = self.store.download_add(&r);
                self.enqueue(key, r.pinned);
            }
        }
    }

    /// Pauses everything: clears the queue and cancels running downloads.
    pub fn stop_downloads(&self) {
        self.downloads.generation.fetch_add(1, Ordering::Relaxed);
        self.downloads.pending.lock().clear();
        self.downloads.retry.lock().clear();
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

    /// Cache files the janitor must not touch: saved downloads still in the cache (unfinished)
    /// and whatever is downloading now. (Finished saved downloads are outside the cache.)
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
