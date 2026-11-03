//! dango core: metadata, provider runtime, source resolution, streaming cache.
//! The desktop shell (`app/`) is a thin layer over [`Dango`].

pub mod anilist;
pub mod downloads;
pub mod http;
pub mod janitor;
pub mod js;
pub mod providers;
pub mod registry;
pub mod resolver;
pub mod settings;
pub mod store;
pub mod stream;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::{json, Value};

use anilist::{Anilist, MediaTitles};
use http::Http;
use providers::Providers;
use resolver::{PlayableSource, ProbeSnapshot, Resolver};
use settings::Settings;
use store::{LibraryEntry, Progress, Store};
use stream::{StreamInner, StreamServer};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Paths {
    pub data: PathBuf,
    pub cache: PathBuf,
}

impl Paths {
    /// `DANGO_DATA_DIR`, then a `data/` folder next to the executable when a `portable` marker
    /// file exists there, then the OS-standard locations.
    pub fn resolve() -> Result<Self> {
        if let Ok(d) = std::env::var("DANGO_DATA_DIR") {
            let d = PathBuf::from(d);
            return Ok(Self { cache: d.join("cache"), data: d });
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            if dir.join("portable").exists() {
                let d = dir.join("data");
                return Ok(Self { cache: d.join("cache"), data: d });
            }
        }
        let pd = directories::ProjectDirs::from("dev", "dango", "dango").context("no home directory")?;
        Ok(Self { data: pd.data_dir().to_path_buf(), cache: pd.cache_dir().to_path_buf() })
    }
}

pub struct Dango {
    pub paths: Paths,
    pub store: Arc<Store>,
    pub settings: Arc<RwLock<Settings>>,
    pub anilist: Arc<Anilist>,
    pub providers: Arc<Providers>,
    pub stream: Arc<StreamInner>,
    pub resolver: Arc<Resolver>,
    pub http: Http,
    streams_cache: moka::sync::Cache<String, Arc<Vec<PlayableSource>>>,
    downloads: downloads::Queue,
    /// Background work goes through this handle: callers like Tauri's sync commands run on the
    /// UI thread, outside the runtime, where `tokio::spawn` panics.
    rt: tokio::runtime::Handle,
}

const IMAGE_KEYS: [&str; 5] = ["extraLarge", "large", "medium", "bannerImage", "thumbnail"];

impl Dango {
    pub async fn start(paths: Paths) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&paths.data)?;
        std::fs::create_dir_all(&paths.cache)?;
        let store = Arc::new(Store::open(&paths.data.join("dango.db"))?);
        let settings: Settings =
            store.setting("settings").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let settings = Arc::new(RwLock::new(settings.normalized()));
        let http = Http::new();
        let anilist = Anilist::new(http.clone(), store.clone());
        js::init_host(http.clone(), anilist.clone());
        let providers = Providers::new(paths.data.join("remote-providers"), http.clone(), VERSION);
        let server = StreamServer::start(&paths.cache, http.clone(), providers.clone(), settings.clone()).await?;
        let stream = server.inner.clone();
        let resolver = Resolver::new(providers.clone(), store.clone(), stream.clone(), settings.clone());
        let me = Arc::new(Self {
            paths,
            store,
            settings,
            anilist,
            providers,
            stream,
            resolver,
            http,
            streams_cache: moka::sync::Cache::builder()
                .max_capacity(500)
                .time_to_live(Duration::from_secs(90 * 60))
                .build(),
            downloads: Default::default(),
            rt: tokio::runtime::Handle::current(),
        });
        me.spawn_background();
        me.spawn_downloader();
        Ok(me)
    }

    fn registry_url(&self) -> String {
        self.settings
            .read()
            .registry_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| registry::DEFAULT_REGISTRY.to_string())
    }

    fn spawn_background(self: &Arc<Self>) {
        let me = self.clone();
        self.rt.spawn(async move {
            loop {
                if let Err(e) = me.providers.refresh(&me.registry_url()).await {
                    tracing::warn!("provider refresh failed: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
            }
        });
        let me = self.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            loop {
                me.sweep_cache().await;
                me.store.kv_purge();
                tokio::time::sleep(Duration::from_secs(10 * 60)).await;
            }
        });
    }

    /// Expires old downloads, then trims the cache to its age/size limits. Saved downloads
    /// are never touched.
    pub async fn sweep_cache(&self) -> janitor::SweepReport {
        self.expire_downloads().await;
        let (limit, age) = {
            let s = self.settings.read();
            (s.cache_limit_mb * 1024 * 1024, Duration::from_secs(s.cache_max_age_hours * 3600))
        };
        let dir = self.paths.cache.clone();
        let keep = self.protected_files();
        let r = tokio::task::spawn_blocking(move || janitor::sweep(&dir, limit, age, &keep)).await.unwrap_or_default();
        if r.removed > 0 {
            tracing::info!(removed = r.removed, freed = r.freed, "cache sweep");
            self.refresh_disk_progress().await;
        }
        r
    }

    /// Waits until the first provider load attempt finished (or the timeout passes).
    pub async fn wait_providers(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.providers.loaded_at() == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ---------------- settings ----------------

    pub fn get_settings(&self) -> Settings {
        self.settings.read().clone()
    }

    pub async fn set_settings(self: &Arc<Self>, s: Settings) -> Result<Settings> {
        let s = s.normalized();
        let registry_changed = self.settings.read().registry_url != s.registry_url;
        self.store.set_setting("settings", &serde_json::to_string(&s)?)?;
        *self.settings.write() = s.clone();
        if registry_changed {
            let me = self.clone();
            self.rt.spawn(async move {
                let _ = me.providers.refresh(&me.registry_url()).await;
            });
        }
        Ok(s)
    }

    // ---------------- metadata ----------------

    fn localize_images(&self, v: &mut Value) {
        match v {
            Value::Object(map) => {
                for (k, val) in map.iter_mut() {
                    if IMAGE_KEYS.contains(&k.as_str()) {
                        if let Some(s) = val.as_str().filter(|s| s.starts_with("http")) {
                            *val = Value::String(self.stream.img_url(s));
                            continue;
                        }
                    }
                    self.localize_images(val);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.localize_images(x)),
            _ => {}
        }
    }

    pub async fn home(&self) -> Result<Value> {
        let mut v = self.anilist.home().await?;
        self.localize_images(&mut v);
        Ok(v)
    }

    /// AniList search. The top results are prewarmed: every provider resolves its id and
    /// episode list in the background so opening a result is instant.
    pub async fn search(&self, q: &str, page: u32) -> Result<Value> {
        let adult = self.settings.read().show_mature;
        let mut v = self.anilist.search(q, page, adult).await?;
        if page <= 1 {
            for m in v["media"].as_array().into_iter().flatten().take(3) {
                self.resolver.prewarm(MediaTitles::from_media(m));
            }
        }
        self.localize_images(&mut v);
        Ok(v)
    }

    pub async fn details(&self, id: i64) -> Result<Value> {
        let mut v = self.anilist.details(id).await?;
        self.localize_images(&mut v);
        v["inLibrary"] = json!(self.store.in_library(id));
        v["progress"] = serde_json::to_value(self.store.progress_for(id)?)?;
        Ok(v)
    }

    async fn titles(&self, id: i64) -> Result<MediaTitles> {
        self.anilist.titles(id).await
    }

    // ---------------- sources & playback ----------------

    pub async fn probe(&self, id: i64) -> Result<ProbeSnapshot> {
        self.wait_providers(Duration::from_secs(20)).await;
        let t = self.titles(id).await?;
        Ok(self.resolver.probe(t, true))
    }

    pub fn subscribe_probes(&self) -> tokio::sync::broadcast::Receiver<ProbeSnapshot> {
        self.resolver.subscribe()
    }

    pub async fn episodes(&self, id: i64, provider: &str) -> Result<Vec<resolver::Episode>> {
        let t = self.titles(id).await?;
        self.resolver.episodes(&t, provider).await
    }

    /// Playable sources for an episode. Results are cached so the player gets the same
    /// (often signed) URLs a background download already fetched; `refresh` bypasses that.
    pub async fn streams(
        self: &Arc<Self>,
        id: i64,
        provider: &str,
        episode: &str,
        refresh: bool,
    ) -> Result<Vec<PlayableSource>> {
        let key = format!("{id}:{provider}:{episode}:{}", self.settings.read().mode);
        if !refresh {
            if let Some(v) = self.streams_cache.get(&key) {
                return Ok((*v).clone());
            }
        }
        let t = self.titles(id).await?;
        let v = self.resolver.streams(&t, provider, episode).await?;
        self.streams_cache.insert(key, Arc::new(v.clone()));
        Ok(v)
    }

    /// Resolves the next episode's sources ahead of time (called near the end of playback).
    pub fn prefetch_next(self: &Arc<Self>, id: i64, provider: String, episode: String) {
        if !self.settings.read().prefetch_next_episode {
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move {
            let _ = me.streams(id, &provider, &episode, false).await;
        });
    }

    /// Opening/ending timestamps from aniskip (needs the MAL id).
    pub async fn skip_times(&self, mal_id: i64, episode: &str, duration: f64) -> Result<Value> {
        let ep = episode.split('.').next().unwrap_or(episode);
        let url = format!(
            "https://api.aniskip.com/v2/skip-times/{mal_id}/{ep}?types[]=op&types[]=ed&episodeLength={}",
            duration.max(0.0) as u64
        );
        let key = format!("skip:{mal_id}:{ep}");
        if let Some(v) = self.store.kv_get(&key).and_then(|s| serde_json::from_str(&s).ok()) {
            return Ok(v);
        }
        let r = self.http.client().get(&url).timeout(Duration::from_secs(10)).send().await?;
        let v: Value = if r.status().is_success() { r.json().await? } else { json!({ "found": false, "results": [] }) };
        self.store.kv_set(&key, &v.to_string(), 7 * 86_400);
        Ok(v)
    }

    // ---------------- library & progress ----------------

    pub fn library(&self) -> Result<Vec<LibraryEntry>> {
        let mut l = self.store.library()?;
        for e in &mut l {
            e.cover = e.cover.as_deref().map(|c| self.localize_url(c));
        }
        Ok(l)
    }

    fn localize_url(&self, u: &str) -> String {
        if u.starts_with("http") && !u.starts_with("http://127.0.0.1") {
            self.stream.img_url(u)
        } else {
            u.to_string()
        }
    }

    /// Strips the local proxy wrapper so stored cover URLs survive restarts (new port/token).
    fn original_image(&self, u: &str) -> String {
        if let Some(data) = u.strip_prefix("http://127.0.0.1").and_then(|r| r.split("/img/").nth(1)) {
            use base64::Engine;
            if let Ok(b) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data) {
                if let Ok(spec) = serde_json::from_slice::<stream::Upstream>(&b) {
                    return spec.url;
                }
            }
        }
        u.to_string()
    }

    pub fn library_add(&self, id: i64, title: &str, cover: Option<&str>) -> Result<()> {
        let cover = cover.map(|c| self.original_image(c));
        self.store.library_add(id, title, cover.as_deref())
    }

    pub fn library_remove(&self, id: i64) -> Result<()> {
        self.store.library_remove(id)
    }

    pub fn save_progress(&self, mut p: Progress) -> Result<()> {
        p.cover = p.cover.map(|c| self.original_image(&c));
        self.store.save_progress(&p)
    }

    pub fn continue_watching(&self) -> Result<Vec<Progress>> {
        let mut v = self.store.continue_watching(24)?;
        for p in &mut v {
            p.cover = p.cover.as_deref().map(|c| self.localize_url(c));
        }
        Ok(v)
    }

    pub fn remove_progress(&self, id: i64) -> Result<()> {
        self.store.remove_progress(id)
    }

    // ---------------- providers & cache ----------------

    pub fn provider_catalog(&self) -> Vec<providers::CatalogItem> {
        self.providers.catalog()
    }

    pub async fn refresh_providers(&self) -> Result<()> {
        self.providers.refresh(&self.registry_url()).await
    }

    pub async fn cache_stats(&self) -> janitor::CacheStats {
        let dir = self.paths.cache.clone();
        tokio::task::spawn_blocking(move || janitor::stats(&dir)).await.unwrap_or_default()
    }

    /// Clears the caches. Downloads that aren't saved go too; saved ones stay.
    pub async fn clear_cache(&self, images: bool) -> janitor::SweepReport {
        self.stop_downloads();
        for r in self.store.downloads().unwrap_or_default().into_iter().filter(|r| !r.pinned) {
            let _ = self.delete_download(&r.key).await;
        }
        self.stream.clear_memory();
        self.store.kv_clear();
        js::clear_provider_cache();
        let dir = self.paths.cache.clone();
        let keep = self.protected_files();
        let r = tokio::task::spawn_blocking(move || janitor::clear(&dir, images, &keep)).await.unwrap_or_default();
        self.refresh_disk_progress().await;
        self.resume_downloads();
        r
    }
}

/// Installs a tracing subscriber honouring `RUST_LOG` (default `info` for dango crates).
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,dango=info,dango_core=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).try_init();
}
