use serde::{Deserialize, Serialize};

/// User settings persisted as one JSON document in the `settings` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// "sub" or "dub".
    pub mode: String,
    pub show_mature: bool,
    pub disabled_providers: Vec<String>,
    /// Provider tried first when it works for a show.
    pub preferred_provider: Option<String>,
    /// Upper bound for the on-disk stream cache.
    pub cache_limit_mb: u64,
    /// Stream cache entries older than this are removed.
    pub cache_max_age_hours: u64,
    /// Download the rest of the episode in the background while playing.
    pub download_ahead: bool,
    /// Parallel segment downloads per episode.
    pub download_concurrency: usize,
    /// Also warm up the next episode near the end of the current one.
    pub prefetch_next_episode: bool,
    pub autoplay_next: bool,
    pub auto_skip_intro: bool,
    pub registry_url: Option<String>,
    /// cf_clearance cookie for providers that sit behind Cloudflare (animepahe).
    pub cf_clearance: Option<String>,
    /// User-agent that obtained cf_clearance.
    pub cf_user_agent: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "sub".into(),
            show_mature: false,
            disabled_providers: vec![],
            preferred_provider: None,
            cache_limit_mb: 4096,
            cache_max_age_hours: 72,
            download_ahead: true,
            download_concurrency: 4,
            prefetch_next_episode: true,
            autoplay_next: true,
            auto_skip_intro: false,
            registry_url: None,
            cf_clearance: None,
            cf_user_agent: None,
        }
    }
}

impl Settings {
    pub fn normalized(mut self) -> Self {
        if self.mode != "dub" {
            self.mode = "sub".into();
        }
        self.cache_limit_mb = self.cache_limit_mb.clamp(256, 512 * 1024);
        self.cache_max_age_hours = self.cache_max_age_hours.clamp(1, 24 * 90);
        self.download_concurrency = self.download_concurrency.clamp(1, 12);
        self.disabled_providers = self.disabled_providers.into_iter().map(|s| s.to_lowercase()).collect();
        self
    }
}
