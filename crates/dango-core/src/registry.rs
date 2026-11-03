//! Remote provider registry: parsing, SHA-256 verified downloads, disk cache and fallbacks.
//! Port of server/src/providers/remote-types.ts + remote-loader.ts from the Node version.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::http::{Http, GENERIC_UA};

pub const DEFAULT_REGISTRY: &str = "https://raw.githubusercontent.com/serifpersia/dango-providers/main/registry.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyRule {
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub suffixes: Vec<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub origin_from_referer: bool,
    #[serde(default)]
    pub scrape: bool,
    #[serde(default)]
    pub cf_cookie: bool,
    #[serde(default)]
    pub mirrors: Vec<String>,
    #[serde(default)]
    pub embed_patch: bool,
}

impl ProxyRule {
    pub fn matches(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.hosts.contains(&host) || self.suffixes.iter().any(|s| host.ends_with(s.as_str()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEntry {
    pub id: String,
    pub label: String,
    pub version: String,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub embed_base: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub min_dango: Option<String>,
    #[serde(default)]
    pub mature: bool,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub sub: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub modes: Option<Vec<String>>,
    #[serde(default)]
    pub proxy: Option<ProxyRule>,
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default = "yes")]
    pub enabled_by_default: bool,
}

fn yes() -> bool {
    true
}

impl ProviderEntry {
    pub fn kind(&self) -> &str {
        if self.embed_base.is_some() {
            "tv"
        } else {
            self.kind.as_deref().unwrap_or("anime")
        }
    }

    pub fn file_name(&self) -> String {
        format!("{}-{}-{}.mjs", self.id.to_lowercase(), self.version, self.sha256.as_deref().unwrap_or("nosha"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub version: u64,
    #[serde(default)]
    pub updated_at: Option<String>,
    pub providers: Vec<ProviderEntry>,
}

fn valid_semver(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// True when `current >= minimum` (both major.minor.patch).
pub fn meets_min(current: &str, minimum: &str) -> bool {
    let p = |s: &str| s.split('.').map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(current) >= p(minimum)
}

pub fn parse_registry(bytes: &[u8]) -> Result<Registry> {
    let mut r: Registry = serde_json::from_slice(bytes).context("registry json")?;
    let mut seen = std::collections::HashSet::new();
    for (i, p) in r.providers.iter_mut().enumerate() {
        p.id = p.id.trim().to_string();
        if p.id.is_empty() || !p.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("registry.providers[{i}].id is invalid");
        }
        if !valid_semver(p.version.trim()) {
            bail!("registry.providers[{i}].version must be major.minor.patch");
        }
        if let Some(s) = &p.sha256 {
            if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("registry.providers[{i}].sha256 must be 64 hex chars");
            }
            p.sha256 = Some(s.to_ascii_lowercase());
        }
        if p.entry.as_deref().map(str::trim).unwrap_or("").is_empty() && p.embed_base.is_none() {
            bail!("registry.providers[{i}].entry must be a non-empty string");
        }
        if let Some(rule) = &mut p.proxy {
            for v in rule.hosts.iter_mut().chain(rule.suffixes.iter_mut()).chain(rule.mirrors.iter_mut()) {
                *v = v.trim().to_ascii_lowercase();
            }
        }
        if !seen.insert(p.id.to_lowercase()) {
            bail!("registry: duplicate provider id {}", p.id);
        }
    }
    Ok(r)
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub struct RegistryStore {
    dir: PathBuf,
    url: String,
    http: Http,
}

pub struct LoadedModule {
    pub entry: ProviderEntry,
    pub source: String,
    /// Set when the module is a cached fallback because the update failed.
    pub fallback_error: Option<String>,
}

pub struct LoadOutcome {
    pub registry: Registry,
    pub modules: Vec<LoadedModule>,
    pub failures: Vec<(ProviderEntry, String)>,
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", rand::random::<u32>()));
    tokio::fs::write(&tmp, bytes).await?;
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok(())
}

impl RegistryStore {
    pub fn new(dir: PathBuf, url: String, http: Http) -> Self {
        Self { dir, url, http }
    }

    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        if url.starts_with("http://") || url.starts_with("https://") {
            let r = self
                .http
                .client()
                .get(url)
                .header("user-agent", GENERIC_UA)
                .timeout(Duration::from_secs(15))
                .send()
                .await?;
            if !r.status().is_success() {
                bail!("HTTP {} for {url}", r.status());
            }
            Ok(r.bytes().await?.to_vec())
        } else {
            Ok(tokio::fs::read(url.trim_start_matches("file://")).await?)
        }
    }

    fn resolve_entry(&self, entry: &str) -> String {
        let e = entry.trim();
        if e.starts_with("http://") || e.starts_with("https://") || e.starts_with("file://") {
            return e.to_string();
        }
        if let Ok(base) = url::Url::parse(&self.url) {
            if let Ok(u) = base.join(e) {
                return u.to_string();
            }
        }
        Path::new(&self.url).parent().unwrap_or(Path::new(".")).join(e).to_string_lossy().into_owned()
    }

    async fn read_verified(&self, entry: &ProviderEntry) -> Option<String> {
        let sha = entry.sha256.as_deref()?;
        let bytes = tokio::fs::read(self.dir.join(entry.file_name())).await.ok()?;
        (sha256_hex(&bytes) == sha).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn load_entry(&self, entry: &ProviderEntry, app_version: &str) -> Result<String> {
        if let Some(min) = &entry.min_dango {
            if !meets_min(app_version, min) {
                bail!("requires dango {min}, running {app_version}");
            }
        }
        if let Some(src) = self.read_verified(entry).await {
            return Ok(src);
        }
        let sha = entry.sha256.as_deref().ok_or_else(|| anyhow!("missing sha256 for {}", entry.id))?;
        let url = self.resolve_entry(entry.entry.as_deref().unwrap_or_default());
        let bytes = self.fetch_bytes(&url).await.map_err(|e| anyhow!("download failed: {e}"))?;
        if sha256_hex(&bytes) != sha {
            bail!("sha256 mismatch for {}", entry.id);
        }
        write_atomic(&self.dir.join(entry.file_name()), &bytes).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn read_json(&self, name: &str) -> Option<Registry> {
        let b = tokio::fs::read(self.dir.join(name)).await.ok()?;
        parse_registry(&b).ok()
    }

    /// Fetches the registry (falling back to the cached copy), downloads and verifies the
    /// modules matching `want`, and falls back to the last verified version per provider.
    pub async fn load(&self, app_version: &str, want: impl Fn(&ProviderEntry) -> bool) -> Result<LoadOutcome> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let previous = self.read_json("active.json").await;
        let registry = match self.fetch_bytes(&self.url).await.and_then(|b| parse_registry(&b)) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("registry fetch failed ({e:#}); using disk cache");
                self.read_json("registry.json").await.ok_or_else(|| anyhow!("no registry available: {e:#}"))?
            }
        };

        let wanted: Vec<ProviderEntry> = registry.providers.iter().filter(|p| want(p)).cloned().collect();
        let results = futures::future::join_all(wanted.iter().map(|e| self.load_entry(e, app_version))).await;

        let mut modules = Vec::new();
        let mut failures = Vec::new();
        let mut active = Vec::new();
        for (entry, res) in wanted.into_iter().zip(results) {
            match res {
                Ok(source) => {
                    active.push(entry.clone());
                    modules.push(LoadedModule { entry, source, fallback_error: None });
                }
                Err(e) => {
                    let prev = previous
                        .as_ref()
                        .and_then(|r| r.providers.iter().find(|p| p.id.eq_ignore_ascii_case(&entry.id)))
                        .cloned();
                    match prev {
                        Some(p) => match self.read_verified(&p).await {
                            Some(source) => {
                                active.push(p.clone());
                                modules.push(LoadedModule {
                                    entry: p.clone(),
                                    source,
                                    fallback_error: Some(format!("serving cached {}: {e:#}", p.version)),
                                });
                            }
                            None => failures.push((entry, format!("{e:#}"))),
                        },
                        None => failures.push((entry, format!("{e:#}"))),
                    }
                }
            }
        }

        let active_reg =
            Registry { version: registry.version, updated_at: registry.updated_at.clone(), providers: active.clone() };
        write_atomic(&self.dir.join("active.json"), serde_json::to_vec(&active_reg)?.as_slice()).await?;
        write_atomic(&self.dir.join("registry.json"), serde_json::to_vec(&registry)?.as_slice()).await?;

        // Prune superseded module files.
        let keep: std::collections::HashSet<String> =
            registry.providers.iter().chain(active.iter()).map(|p| p.file_name()).collect();
        if let Ok(mut rd) = tokio::fs::read_dir(&self.dir).await {
            while let Ok(Some(f)) = rd.next_entry().await {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.ends_with(".mjs") && !keep.contains(&name) {
                    let _ = tokio::fs::remove_file(f.path()).await;
                }
            }
        }

        Ok(LoadOutcome { registry, modules, failures })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_version() {
        assert!(meets_min("4.0.0", "3.9.1"));
        assert!(meets_min("3.9.1", "3.9.1"));
        assert!(!meets_min("3.9.0", "3.10.0"));
    }

    #[test]
    fn parses_registry() {
        let r = parse_registry(
            br#"{"version":1,"providers":[{"id":"a","label":"A","version":"1.0.0","entry":"a.mjs",
            "proxy":{"suffixes":[".X.top"]}}]}"#,
        )
        .unwrap();
        assert_eq!(r.providers[0].kind(), "anime");
        assert!(r.providers[0].proxy.as_ref().unwrap().matches("cdn.x.top"));
        assert!(parse_registry(
            br#"{"version":1,"providers":[{"id":"a b","label":"A","version":"1.0.0","entry":"x"}]}"#
        )
        .is_err());
    }
}
