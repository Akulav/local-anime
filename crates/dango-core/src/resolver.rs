//! Source discovery: for a show, ask every provider in parallel for its id and episode
//! list, verify that a stream actually plays, and rank the working ones.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Semaphore};

use crate::anilist::MediaTitles;
use crate::providers::{Provider, Providers};
use crate::settings::Settings;
use crate::store::Store;
use crate::stream::{StreamInner, Upstream};

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(25);
const EPISODES_TIMEOUT: Duration = Duration::from_secs(25);
const STREAMS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SourceState {
    Pending,
    Resolving,
    Verifying,
    Ok,
    Missing,
    Auth,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub provider: String,
    pub label: String,
    pub state: SourceState,
    pub show_id: Option<String>,
    pub episode_count: usize,
    /// Verified that a stream for the first episode responds (None = not checked yet).
    pub playable: Option<bool>,
    pub embed_only: bool,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
    pub sub: Option<String>,
    pub tier: Option<String>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub number: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeSnapshot {
    pub anilist_id: i64,
    pub mode: String,
    pub sources: Vec<SourceStatus>,
    pub best: Option<String>,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Quality {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Subtitle {
    pub label: String,
    pub lang: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayableSource {
    pub provider: String,
    pub name: String,
    /// "hls", "mp4" or "iframe".
    pub kind: String,
    pub qualities: Vec<Quality>,
    pub subtitles: Vec<Subtitle>,
}

struct Probe {
    started: Instant,
    expected: Option<i64>,
    statuses: RwLock<Vec<SourceStatus>>,
    episodes: RwLock<HashMap<String, Vec<Episode>>>,
    done: RwLock<bool>,
    /// Stream verification was requested (by opening the show).
    verify: std::sync::atomic::AtomicBool,
    verifying: std::sync::atomic::AtomicBool,
}

pub struct Resolver {
    providers: Arc<Providers>,
    store: Arc<Store>,
    stream: Arc<StreamInner>,
    settings: Arc<RwLock<Settings>>,
    probes: Mutex<HashMap<(i64, String), Arc<Probe>>>,
    events: broadcast::Sender<ProbeSnapshot>,
    /// Bounds background prewarming so it never starves interactive work.
    prewarm: Arc<Semaphore>,
}

fn targets(t: &MediaTitles) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for s in [t.english.clone(), t.romaji.clone(), t.native.clone()]
        .into_iter()
        .flatten()
        .chain(t.synonyms.iter().take(2).cloned())
    {
        if !s.trim().is_empty() && !v.iter().any(|x| x.eq_ignore_ascii_case(s.trim())) {
            v.push(s.trim().to_string());
        }
    }
    v
}

fn parse_episodes(v: &Value) -> Vec<Episode> {
    let titles: HashMap<String, String> = v["availableEpisodesDetail"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some((
                        e["number"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| e["number"].as_f64().map(|n| n.to_string()))?,
                        e["title"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut eps: Vec<Episode> = v["episodes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string).or_else(|| e.as_f64().map(|n| n.to_string())))
                .map(|n| Episode { title: titles.get(&n).cloned(), number: n })
                .collect()
        })
        .unwrap_or_default();
    eps.sort_by(|a, b| {
        let fa = a.number.parse::<f64>().unwrap_or(f64::MAX);
        let fb = b.number.parse::<f64>().unwrap_or(f64::MAX);
        fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
    });
    eps.dedup_by(|a, b| a.number == b.number);
    eps
}

struct Guard<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for Guard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f()
        }
    }
}

fn scopeguard<F: FnOnce()>(f: F) -> Guard<F> {
    Guard(Some(f))
}

fn is_auth(e: &anyhow::Error) -> bool {
    e.to_string().contains("AUTH_REQUIRED")
}

impl Resolver {
    pub fn new(
        providers: Arc<Providers>,
        store: Arc<Store>,
        stream: Arc<StreamInner>,
        settings: Arc<RwLock<Settings>>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            providers,
            store,
            stream,
            settings,
            probes: Mutex::new(HashMap::new()),
            events,
            prewarm: Arc::new(Semaphore::new(2)),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProbeSnapshot> {
        self.events.subscribe()
    }

    fn eligible(&self) -> Vec<Arc<Provider>> {
        let s = self.settings.read();
        self.providers
            .all()
            .into_iter()
            .filter(|p| s.show_mature || !p.entry.mature)
            .filter(|p| !s.disabled_providers.contains(&p.entry.id.to_lowercase()))
            .filter(|p| p.entry.modes.as_ref().map(|m| m.iter().any(|x| *x == s.mode)).unwrap_or(true))
            .collect()
    }

    fn request_ctx(&self) -> Value {
        let s = self.settings.read();
        json!({ "cookie": s.cf_clearance.clone().unwrap_or_default(), "ua": s.cf_user_agent.clone().unwrap_or_default() })
    }

    fn snapshot(&self, id: i64, mode: &str, probe: &Probe) -> ProbeSnapshot {
        let mut sources = probe.statuses.read().clone();
        sources.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let best = sources
            .iter()
            .find(|s| s.state == SourceState::Ok && s.playable != Some(false))
            .map(|s| s.provider.clone());
        ProbeSnapshot { anilist_id: id, mode: mode.to_string(), sources, best, done: *probe.done.read() }
    }

    fn update(&self, id: i64, mode: &str, probe: &Probe, provider: &str, f: impl FnOnce(&mut SourceStatus)) {
        {
            let mut st = probe.statuses.write();
            if let Some(s) = st.iter_mut().find(|s| s.provider == provider) {
                f(s);
                s.score = self.score(s, probe.expected);
            }
        }
        let _ = self.events.send(self.snapshot(id, mode, probe));
    }

    fn score(&self, s: &SourceStatus, expected: Option<i64>) -> f64 {
        let mut score = match s.state {
            SourceState::Ok => 100.0,
            SourceState::Verifying => 60.0,
            SourceState::Resolving | SourceState::Pending => 10.0,
            SourceState::Auth => 2.0,
            SourceState::Missing => 1.0,
            SourceState::Failed => 0.0,
        };
        match s.playable {
            Some(true) => score += 50.0,
            Some(false) => score -= 80.0,
            None => {}
        }
        if s.embed_only {
            score -= 30.0;
        }
        if let Some(exp) = expected.filter(|e| *e > 0) {
            let cov = (s.episode_count as f64 / exp as f64).min(1.0);
            score += 25.0 * cov;
        }
        if s.tier.as_deref() == Some("direct") {
            score += 5.0;
        }
        if s.sub.as_deref() == Some("soft") {
            score += 3.0;
        }
        if let Some(ms) = s.latency_ms {
            score += 10.0 * (1.0 - (ms as f64 / 15_000.0).min(1.0));
        }
        if self.settings.read().preferred_provider.as_deref() == Some(s.provider.as_str()) {
            score += 40.0;
        }
        score
    }

    /// Returns (and starts if needed) the probe for a show. Results stream through `subscribe()`.
    pub fn probe(self: &Arc<Self>, titles: MediaTitles, verify: bool) -> ProbeSnapshot {
        let mode = self.settings.read().mode.clone();
        let key = (titles.id, mode.clone());
        {
            let probes = self.probes.lock();
            if let Some(p) = probes.get(&key) {
                let empty = p.statuses.read().is_empty();
                if !empty && p.started.elapsed() < Duration::from_secs(30 * 60) {
                    if verify {
                        p.verify.store(true, std::sync::atomic::Ordering::Relaxed);
                        if *p.done.read() {
                            self.clone().spawn_verify(titles.id, mode.clone(), p.clone());
                        }
                    }
                    return self.snapshot(titles.id, &mode, p);
                }
            }
        }
        let providers = self.eligible();
        let probe = Arc::new(Probe {
            started: Instant::now(),
            expected: titles.expected_episodes(),
            statuses: RwLock::new(
                providers
                    .iter()
                    .map(|p| SourceStatus {
                        provider: p.entry.id.to_lowercase(),
                        label: p.entry.label.clone(),
                        state: SourceState::Pending,
                        show_id: None,
                        episode_count: 0,
                        playable: None,
                        embed_only: false,
                        latency_ms: None,
                        error: None,
                        sub: p.entry.sub.clone(),
                        tier: p.entry.tier.clone(),
                        score: 0.0,
                    })
                    .collect(),
            ),
            episodes: RwLock::new(HashMap::new()),
            done: RwLock::new(false),
            verify: std::sync::atomic::AtomicBool::new(verify),
            verifying: std::sync::atomic::AtomicBool::new(false),
        });
        self.probes.lock().insert(key, probe.clone());
        let snap = self.snapshot(titles.id, &mode, &probe);
        let this = self.clone();
        tokio::spawn(async move { this.run_probe(titles, mode, probe, providers, verify).await });
        snap
    }

    /// Background warm-up for search results: resolve ids and episode lists only.
    pub fn prewarm(self: &Arc<Self>, titles: MediaTitles) {
        let this = self.clone();
        let sem = self.prewarm.clone();
        tokio::spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else { return };
            let deadline = Instant::now() + Duration::from_secs(30);
            while this.providers.loaded_at() == 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let mode = this.settings.read().mode.clone();
            if this.probes.lock().get(&(titles.id, mode)).is_some_and(|p| !p.statuses.read().is_empty()) {
                return;
            }
            let snap = this.probe(titles, false);
            // Hold the permit until the probe settles so prewarming stays bounded.
            let mut rx = this.subscribe();
            let deadline = Instant::now() + Duration::from_secs(40);
            while Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                    Ok(Ok(s)) if s.anilist_id == snap.anilist_id && s.done => break,
                    Ok(Err(broadcast::error::RecvError::Closed)) => break,
                    _ => {}
                }
            }
        });
    }

    async fn run_probe(
        self: Arc<Self>,
        titles: MediaTitles,
        mode: String,
        probe: Arc<Probe>,
        providers: Vec<Arc<Provider>>,
        verify: bool,
    ) {
        let id = titles.id;
        let tasks = providers.into_iter().map(|p| {
            let this = self.clone();
            let titles = titles.clone();
            let mode = mode.clone();
            let probe = probe.clone();
            async move {
                let pid = p.entry.id.to_lowercase();
                this.update(id, &mode, &probe, &pid, |s| s.state = SourceState::Resolving);
                let started = Instant::now();
                let res = this.resolve_and_list(&p, &titles, &mode).await;
                let ms = started.elapsed().as_millis() as u64;
                match res {
                    Ok((show_id, eps)) if !eps.is_empty() => {
                        let n = eps.len();
                        probe.episodes.write().insert(pid.clone(), eps);
                        this.update(id, &mode, &probe, &pid, |s| {
                            s.state = SourceState::Ok;
                            s.show_id = Some(show_id);
                            s.episode_count = n;
                            s.latency_ms = Some(ms);
                        });
                    }
                    Ok((_, _)) => this.update(id, &mode, &probe, &pid, |s| {
                        s.state = SourceState::Missing;
                        s.error = Some("not found".into());
                        s.latency_ms = Some(ms);
                    }),
                    Err(e) => this.update(id, &mode, &probe, &pid, |s| {
                        s.state = if is_auth(&e) { SourceState::Auth } else { SourceState::Failed };
                        s.error = Some(if is_auth(&e) {
                            "needs Cloudflare cookie (Settings)".into()
                        } else {
                            format!("{e:#}")
                        });
                        s.latency_ms = Some(ms);
                    }),
                }
            }
        });
        futures::future::join_all(tasks).await;
        *probe.done.write() = true;
        let _ = self.events.send(self.snapshot(id, &mode, &probe));
        if verify || probe.verify.load(std::sync::atomic::Ordering::Relaxed) {
            self.spawn_verify(id, mode, probe);
        }
    }

    fn spawn_verify(self: Arc<Self>, id: i64, mode: String, probe: Arc<Probe>) {
        use std::sync::atomic::Ordering;
        let pending = probe.statuses.read().iter().any(|s| s.state == SourceState::Ok && s.playable.is_none());
        if !pending || probe.verifying.swap(true, Ordering::AcqRel) {
            return;
        }
        tokio::spawn(async move {
            let probe2 = probe.clone();
            let _reset = scopeguard(move || probe2.verifying.store(false, Ordering::Release));
            let candidates: Vec<(String, String)> = probe
                .statuses
                .read()
                .iter()
                .filter(|s| s.state == SourceState::Ok && s.playable.is_none())
                .filter_map(|s| Some((s.provider.clone(), s.show_id.clone()?)))
                .collect();
            let tasks = candidates.into_iter().map(|(pid, show_id)| {
                let this = self.clone();
                let probe = probe.clone();
                let mode = mode.clone();
                async move {
                    let first = probe.episodes.read().get(&pid).and_then(|e| e.first().map(|e| e.number.clone()));
                    let Some(ep) = first else { return };
                    this.update(id, &mode, &probe, &pid, |s| s.state = SourceState::Verifying);
                    let started = Instant::now();
                    let r = this.verify_provider(&pid, &show_id, &ep, &mode).await;
                    let ms = started.elapsed().as_millis() as u64;
                    this.update(id, &mode, &probe, &pid, |s| {
                        s.state = SourceState::Ok;
                        match r {
                            Ok(embed_only) => {
                                s.playable = Some(true);
                                s.embed_only = embed_only;
                                s.latency_ms = Some(s.latency_ms.unwrap_or(0).max(ms));
                            }
                            Err(e) => {
                                s.playable = Some(false);
                                s.error = Some(format!("stream check failed: {e:#}"));
                            }
                        }
                    });
                }
            });
            futures::future::join_all(tasks).await;
            let _ = self.events.send(self.snapshot(id, &mode, &probe));
        });
    }

    /// Returns Ok(embed_only).
    async fn verify_provider(&self, pid: &str, show_id: &str, ep: &str, mode: &str) -> Result<bool> {
        let sources = self.raw_streams(pid, show_id, ep, mode).await?;
        let mut last = anyhow!("no sources");
        let mut has_iframe = false;
        for src in &sources {
            if src.kind == "iframe" {
                has_iframe = true;
                continue;
            }
            if let Some((spec, hls)) = src.first.clone() {
                match tokio::time::timeout(Duration::from_secs(15), self.stream.verify(&spec, hls)).await {
                    Ok(Ok(())) => return Ok(false),
                    Ok(Err(e)) => last = e,
                    Err(_) => last = anyhow!("verify timed out"),
                }
            }
        }
        if has_iframe {
            return Ok(true);
        }
        Err(last)
    }

    async fn resolve_and_list(&self, p: &Provider, titles: &MediaTitles, mode: &str) -> Result<(String, Vec<Episode>)> {
        let show_id = self.show_id(p, titles, mode).await?.ok_or_else(|| anyhow!("not found"))?;
        let eps = self.fetch_episodes(p, &show_id, mode).await?;
        Ok((show_id, eps))
    }

    async fn fetch_episodes(&self, p: &Provider, show_id: &str, mode: &str) -> Result<Vec<Episode>> {
        let key = format!("eps:{}:{show_id}:{mode}", p.id());
        if let Some(v) = self.store.kv_get(&key).and_then(|s| serde_json::from_str::<Vec<Episode>>(&s).ok()) {
            return Ok(v);
        }
        let v = p.js.call("getEpisodes", json!([show_id, mode]), Some(self.request_ctx()), EPISODES_TIMEOUT).await?;
        let eps = parse_episodes(&v);
        if !eps.is_empty() {
            self.store.kv_set(&key, &serde_json::to_string(&eps)?, 30 * 60);
        }
        Ok(eps)
    }

    async fn show_id(&self, p: &Provider, titles: &MediaTitles, mode: &str) -> Result<Option<String>> {
        let key = format!("sid:{}:{}:{mode}", p.id(), titles.id);
        if let Some(v) = self.store.kv_get(&key) {
            return Ok(Some(v).filter(|v| !v.is_empty()));
        }
        let aid = titles.id.to_string();
        let ctx = Some(self.request_ctx());
        let mut found: Option<String> = None;
        if p.js.caps.is_direct_id {
            if let Ok(Value::Bool(true)) =
                p.js.call("isDirectId", json!([aid]), ctx.clone(), Duration::from_secs(5)).await
            {
                found = Some(aid.clone());
            }
        }
        let targets = targets(titles);
        if found.is_none() && p.js.caps.resolve_show_id {
            for t in targets.iter().take(3) {
                match p.js.call("resolveShowId", json!([t, titles.romaji, mode]), ctx.clone(), RESOLVE_TIMEOUT).await {
                    Ok(Value::String(s)) if !s.is_empty() => {
                        found = Some(s);
                        break;
                    }
                    Ok(_) => {}
                    Err(e) if is_auth(&e) => return Err(e),
                    Err(e) => tracing::debug!(provider = %p.id(), "resolveShowId failed: {e:#}"),
                }
            }
        }
        if found.is_none() {
            match p.js.call("__resolveBySearch", json!([targets]), ctx, RESOLVE_TIMEOUT).await {
                Ok(Value::String(s)) if !s.is_empty() => found = Some(s),
                Ok(_) => {}
                Err(e) if is_auth(&e) => return Err(e),
                Err(e) => tracing::debug!(provider = %p.id(), "search fallback failed: {e:#}"),
            }
        }
        // Remember misses briefly, hits for a week.
        self.store.kv_set(&key, found.as_deref().unwrap_or(""), if found.is_some() { 7 * 86_400 } else { 3600 });
        Ok(found)
    }

    /// Episodes as reported by a provider (from the probe if available).
    pub async fn episodes(&self, titles: &MediaTitles, provider: &str) -> Result<Vec<Episode>> {
        let mode = self.settings.read().mode.clone();
        if let Some(p) = self.probes.lock().get(&(titles.id, mode.clone())).cloned() {
            if let Some(e) = p.episodes.read().get(provider) {
                return Ok(e.clone());
            }
        }
        let p = self.providers.get(provider).ok_or_else(|| anyhow!("unknown provider {provider}"))?;
        let (_, eps) = self.resolve_and_list(&p, titles, &mode).await?;
        Ok(eps)
    }

    async fn raw_streams(&self, pid: &str, show_id: &str, ep: &str, mode: &str) -> Result<Vec<RawSource>> {
        let p = self.providers.get(pid).ok_or_else(|| anyhow!("unknown provider {pid}"))?;
        let v =
            p.js.call("getStreamUrls", json!([show_id, ep, mode]), Some(self.request_ctx()), STREAMS_TIMEOUT).await?;
        Ok(normalize_sources(&v))
    }

    /// Playable sources for one episode; every URL points at the local stream server.
    pub async fn streams(&self, titles: &MediaTitles, provider: &str, episode: &str) -> Result<Vec<PlayableSource>> {
        let mode = self.settings.read().mode.clone();
        let p = self.providers.get(provider).ok_or_else(|| anyhow!("unknown provider {provider}"))?;
        let show_id = self.show_id(&p, titles, &mode).await?.ok_or_else(|| anyhow!("show not found on {provider}"))?;
        let raw = self.raw_streams(provider, &show_id, episode, &mode).await?;
        let out: Vec<PlayableSource> = raw
            .into_iter()
            .map(|r| PlayableSource {
                provider: provider.to_string(),
                name: r.name,
                kind: r.kind.clone(),
                qualities: r
                    .links
                    .into_iter()
                    .map(|(label, spec, hls)| Quality {
                        label,
                        url: match (r.kind.as_str(), hls) {
                            ("iframe", _) => spec.url.clone(),
                            (_, true) => self.stream.hls_url(&spec),
                            (_, false) => self.stream.file_url(&spec),
                        },
                    })
                    .collect(),
                subtitles: r
                    .subtitles
                    .into_iter()
                    .map(|(label, lang, spec)| Subtitle { label, lang, url: self.stream.sub_url(&spec) })
                    .collect(),
            })
            .filter(|s| !s.qualities.is_empty())
            .collect();
        if out.is_empty() {
            return Err(anyhow!("{provider} returned no playable sources"));
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
struct RawSource {
    name: String,
    kind: String,
    links: Vec<(String, Upstream, bool)>,
    subtitles: Vec<(String, String, Upstream)>,
    first: Option<(Upstream, bool)>,
}

/// Converts a provider link (absolute URL or the legacy `/api/proxy?...` form) to an upstream spec.
fn link_spec(link: &str, headers: &Value) -> Option<Upstream> {
    let h = |name: &str| {
        headers
            .as_object()
            .and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
            .and_then(|(_, v)| v.as_str())
            .map(str::to_string)
    };
    if let Some(q) = link.strip_prefix("/api/proxy?") {
        let params: HashMap<String, String> = url::form_urlencoded::parse(q.as_bytes()).into_owned().collect();
        return Some(Upstream {
            url: params.get("url")?.clone(),
            referer: params.get("referer").cloned().filter(|s| !s.is_empty()).or_else(|| h("referer")),
            origin: params.get("origin").cloned().filter(|s| !s.is_empty()).or_else(|| h("origin")),
            ua: params.get("ua").cloned().filter(|s| !s.is_empty()),
            cookie: params.get("cookie").cloned().filter(|s| !s.is_empty()),
            scrape: params.get("scrape").map(|v| v == "1").unwrap_or(false),
            cf: params.get("cf").map(|v| v == "1").unwrap_or(false),
            variant: params.get("variant").and_then(|v| v.parse().ok()),
        });
    }
    if link.starts_with("http://") || link.starts_with("https://") {
        return Some(Upstream {
            url: link.to_string(),
            referer: h("referer"),
            origin: h("origin"),
            ua: h("user-agent"),
            cookie: h("cookie"),
            ..Default::default()
        });
    }
    if let Some(rest) = link.strip_prefix("//") {
        return link_spec(&format!("https://{rest}"), headers);
    }
    None
}

fn normalize_sources(v: &Value) -> Vec<RawSource> {
    let Some(arr) = v.as_array() else { return vec![] };
    arr.iter()
        .filter_map(|s| {
            let name = s["sourceName"].as_str().unwrap_or("Source").to_string();
            let iframe = s["type"].as_str() == Some("iframe");
            let mut links = Vec::new();
            for l in s["links"].as_array().into_iter().flatten() {
                let Some(link) = l["link"].as_str() else { continue };
                if link.starts_with("/api/embed-proxy") {
                    continue;
                }
                let Some(spec) = link_spec(link, &l["headers"]) else { continue };
                let hls = l["hls"].as_bool().unwrap_or(false) || spec.url.contains(".m3u8");
                let label = l["resolutionStr"].as_str().unwrap_or("Auto").to_string();
                links.push((label, spec, hls));
            }
            if links.is_empty() {
                return None;
            }
            // Highest resolution first, "Auto" last unless it's the only one.
            links.sort_by_key(|(label, _, _)| {
                std::cmp::Reverse(
                    label.trim_end_matches('p').parse::<u32>().unwrap_or(if label.eq_ignore_ascii_case("auto") {
                        1
                    } else {
                        0
                    }),
                )
            });
            let kind = if iframe {
                "iframe"
            } else if links.iter().any(|(_, _, h)| *h) {
                "hls"
            } else {
                "mp4"
            };
            let subtitles = s["subtitles"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| {
                    let url = t["url"].as_str()?;
                    let spec = link_spec(url, &Value::Null)?;
                    let referer = links.first().and_then(|(_, s, _)| s.referer.clone());
                    Some((
                        t["label"].as_str().unwrap_or("Subtitles").to_string(),
                        t["language"].as_str().unwrap_or("en").to_string(),
                        Upstream { referer: spec.referer.or(referer), ..spec },
                    ))
                })
                .collect();
            let first = (!iframe).then(|| {
                let auto = links.iter().find(|(l, _, _)| l.eq_ignore_ascii_case("auto")).or(links.first());
                auto.map(|(_, s, h)| (s.clone(), *h))
            });
            Some(RawSource { name, kind: kind.into(), first: first.flatten(), links, subtitles })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_legacy_proxy_links() {
        let v = json!([{
            "sourceName": "X", "type": "player",
            "links": [
                { "resolutionStr": "720p", "link": "/api/proxy?url=https%3A%2F%2Fcdn.x%2Fa.m3u8&referer=https%3A%2F%2Fr%2F&variant=1", "hls": true },
                { "resolutionStr": "Auto", "link": "https://cdn.x/a.m3u8", "hls": true, "headers": { "Referer": "https://r/" } },
                { "resolutionStr": "1080p", "link": "/api/embed-proxy?url=x" }
            ],
            "subtitles": [{ "language": "en", "label": "English", "url": "https://s/x.vtt" }]
        }]);
        let s = normalize_sources(&v);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, "hls");
        assert_eq!(s[0].links.len(), 2);
        assert_eq!(s[0].links[0].0, "720p");
        assert_eq!(s[0].links[0].1.variant, Some(1));
        assert_eq!(s[0].links[0].1.referer.as_deref(), Some("https://r/"));
        assert_eq!(s[0].subtitles[0].2.referer.as_deref(), Some("https://r/"));
        assert_eq!(s[0].first.as_ref().unwrap().0.variant, None);
    }

    #[test]
    fn sorts_episodes_numerically() {
        let e = parse_episodes(&json!({ "episodes": ["10", "2", "1", "2", "1.5"] }));
        let n: Vec<_> = e.iter().map(|e| e.number.as_str()).collect();
        assert_eq!(n, ["1", "1.5", "2", "10"]);
    }
}
