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
    /// Episodes downloaded in the background when a show is opened (from the resume point).
    pub predownload_episodes: usize,
    pub autoplay_next: bool,
    pub auto_skip_intro: bool,
    pub registry_url: Option<String>,
    /// cf_clearance cookie for providers that sit behind Cloudflare (animepahe).
    pub cf_clearance: Option<String>,
    /// User-agent that obtained cf_clearance.
    pub cf_user_agent: Option<String>,
    /// Look for a new Tsukimi release shortly after start and every 6 hours.
    pub check_updates: bool,
    /// Schema revision for one-off migrations of stored settings. Missing in stored JSON
    /// means 0 (field-level default), unlike the other fields which fall back to `Default`.
    #[serde(default)]
    pub rev: u32,
}

const REV: u32 = 1;

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "sub".into(),
            show_mature: false,
            disabled_providers: vec![],
            preferred_provider: None,
            cache_limit_mb: 4096,
            cache_max_age_hours: 48,
            download_ahead: true,
            download_concurrency: 4,
            prefetch_next_episode: true,
            predownload_episodes: 2,
            autoplay_next: true,
            auto_skip_intro: false,
            registry_url: None,
            cf_clearance: None,
            cf_user_agent: None,
            check_updates: true,
            rev: REV,
        }
    }
}

impl Settings {
    pub fn normalized(mut self) -> Self {
        if self.rev < 1 {
            // rev 1: cache retention default went from 72h to 48h.
            if self.cache_max_age_hours == 72 {
                self.cache_max_age_hours = 48;
            }
        }
        self.rev = REV;
        if self.mode != "dub" {
            self.mode = "sub".into();
        }
        self.cache_limit_mb = self.cache_limit_mb.clamp(256, 512 * 1024);
        self.cache_max_age_hours = self.cache_max_age_hours.clamp(1, 24 * 90);
        self.download_concurrency = self.download_concurrency.clamp(1, 12);
        self.predownload_episodes = self.predownload_episodes.min(12);
        self.disabled_providers = self.disabled_providers.into_iter().map(|s| s.to_lowercase()).collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_retention_default() {
        let s: Settings = serde_json::from_str(r#"{"cacheMaxAgeHours":72}"#).unwrap();
        assert_eq!(s.normalized().cache_max_age_hours, 48);
        let s: Settings = serde_json::from_str(r#"{"cacheMaxAgeHours":72,"rev":1}"#).unwrap();
        assert_eq!(s.normalized().cache_max_age_hours, 72, "explicit choice after migration is kept");
        assert_eq!(Settings::default().normalized().predownload_episodes, 2);
        // settings saved by dango (no checkUpdates) turn update checks on
        let s: Settings = serde_json::from_str(r#"{"mode":"dub","rev":1}"#).unwrap();
        assert!(s.check_updates && s.mode == "dub");
    }
}
