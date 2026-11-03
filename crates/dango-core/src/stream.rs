//! Local streaming proxy + disk cache.
//!
//! The webview plays everything through `http://127.0.0.1:{port}/s/{token}/...`:
//!
//! * `hls`  – playlists are fetched with the provider's headers, rewritten so every child
//!   playlist, key, init map and segment goes back through this server.
//! * `seg`  – segments/keys are served from disk; misses are downloaded once (deduplicated)
//!   and written to the cache. Media playlists start a background job that downloads the
//!   rest of the episode ahead of the playhead.
//! * `file` – progressive files (mp4) download sequentially to disk; range requests inside the
//!   downloaded region are served from disk while the download continues, far seeks are
//!   passed through to the upstream.
//! * `sub`  – subtitles converted to WebVTT.
//! * `img`  – cover/banner images with a persistent disk cache.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Result};
use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode};
use axum::routing::get;
use axum::Router;
use base64::Engine;
use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt, Shared};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{watch, Notify};

use crate::http::{header_map, Http, PROXY_UA};
use crate::providers::Providers;
use crate::settings::Settings;

/// Description of an upstream resource and the headers needed to fetch it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq, Hash)]
pub struct Upstream {
    #[serde(rename = "u")]
    pub url: String,
    #[serde(rename = "r", default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    #[serde(rename = "o", default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(rename = "a", default, skip_serializing_if = "Option::is_none")]
    pub ua: Option<String>,
    #[serde(rename = "c", default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
    #[serde(rename = "s", default, skip_serializing_if = "std::ops::Not::not")]
    pub scrape: bool,
    #[serde(rename = "f", default, skip_serializing_if = "std::ops::Not::not")]
    pub cf: bool,
    #[serde(rename = "v", default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<usize>,
}

impl Upstream {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), ..Default::default() }
    }

    fn encode(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }

    fn decode(s: &str) -> Option<Self> {
        let raw = s.split('.').next()?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn child(&self, url: String) -> Self {
        Self { url, variant: None, ..self.clone() }
    }
}

pub fn sha_key(s: &str) -> String {
    hex::encode(&Sha256::digest(s.as_bytes())[..20])
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStatus {
    pub id: String,
    pub total: usize,
    pub done: usize,
    pub playhead: usize,
    pub bytes: u64,
    pub active: bool,
}

struct HlsJob {
    id: String,
    segs: Vec<Upstream>,
    done: Vec<AtomicBool>,
    claimed: Vec<AtomicBool>,
    playhead: AtomicUsize,
    bytes: AtomicU64,
    cancelled: AtomicBool,
    wake: Notify,
}

impl HlsJob {
    fn done_count(&self) -> usize {
        self.done.iter().filter(|d| d.load(Ordering::Relaxed)).count()
    }

    fn next(&self, window: Option<usize>) -> Option<usize> {
        let start = self.playhead.load(Ordering::Relaxed).min(self.segs.len());
        let end = window.map(|w| (start + w).min(self.segs.len())).unwrap_or(self.segs.len());
        (start..end).find(|&i| {
            !self.done[i].load(Ordering::Relaxed)
                && self.claimed[i].compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        })
    }
}

struct FileJob {
    part: PathBuf,
    done_path: PathBuf,
    len: AtomicU64,
    water: watch::Sender<u64>,
    complete: AtomicBool,
    failed: AtomicBool,
}

type SegFuture = Shared<BoxFuture<'static, Result<(PathBuf, u64), String>>>;

pub struct StreamInner {
    http: Http,
    providers: Arc<Providers>,
    settings: Arc<RwLock<Settings>>,
    seg_dir: PathBuf,
    file_dir: PathBuf,
    img_dir: PathBuf,
    token: String,
    base: RwLock<String>,
    playlists_vod: moka::sync::Cache<String, Arc<String>>,
    playlists_live: moka::sync::Cache<String, Arc<String>>,
    subs: moka::sync::Cache<String, Arc<String>>,
    inflight: Mutex<HashMap<String, SegFuture>>,
    jobs: Mutex<Vec<Arc<HlsJob>>>,
    files: Mutex<HashMap<String, Arc<FileJob>>>,
}

pub struct StreamServer {
    pub inner: Arc<StreamInner>,
    pub addr: SocketAddr,
}

fn is_blocked_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost") || d.ends_with(".localhost"),
        None => true,
    }
}

fn check_url(raw: &str) -> Result<url::Url> {
    let u = url::Url::parse(raw)?;
    if !matches!(u.scheme(), "http" | "https") || is_blocked_host(&u) {
        bail!("blocked upstream url");
    }
    Ok(u)
}

fn origin_of(u: &str) -> Option<String> {
    url::Url::parse(u).ok().map(|u| u.origin().ascii_serialization()).filter(|o| o != "null")
}

fn mirror_urls(url: &str, mirrors: &[String]) -> Vec<String> {
    let mut out = vec![url.to_string()];
    let Ok(cur) = url::Url::parse(url) else { return out };
    let mut seen = vec![cur.host_str().unwrap_or("").to_ascii_lowercase()];
    for m in mirrors {
        if seen.contains(m) {
            continue;
        }
        seen.push(m.clone());
        let mut c = cur.clone();
        if c.set_host(Some(m)).is_ok() {
            out.push(c.to_string());
        }
    }
    out
}

/// Keeps only the `idx`-th variant of a master playlist.
pub fn pin_variant(body: &str, idx: usize) -> Option<String> {
    let mut stream_idx: isize = -1;
    let mut skip_uri = false;
    let mut out = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("#EXT-X-STREAM-INF") {
            stream_idx += 1;
            skip_uri = stream_idx as usize != idx;
            if !skip_uri {
                out.push(line);
            }
            continue;
        }
        if skip_uri && !t.is_empty() && !t.starts_with('#') {
            skip_uri = false;
            continue;
        }
        out.push(line);
    }
    (stream_idx >= 0 && idx as isize <= stream_idx).then(|| out.join("\n"))
}

fn srt_to_vtt(s: &str) -> String {
    let re = regex::Regex::new(r"(\d{2}:\d{2}:\d{2}),(\d{3})").unwrap();
    format!("WEBVTT\n\n{}", re.replace_all(&s.replace("\r\n", "\n"), "$1.$2"))
}

fn ass_to_vtt(s: &str) -> String {
    let tag = regex::Regex::new(r"\{[^}]*\}").unwrap();
    let mut out = String::from("WEBVTT\n\n");
    let mut fmt: Vec<String> = Vec::new();
    let ts = |t: &str| -> String {
        let p: Vec<&str> = t.trim().split([':', '.']).collect();
        if p.len() < 4 {
            return "00:00:00.000".into();
        }
        format!("{:0>2}:{:0>2}:{:0>2}.{:0<3}", p[0], p[1], p[2], p[3])
    };
    for line in s.lines() {
        if let Some(f) = line.strip_prefix("Format:") {
            fmt = f.split(',').map(|x| x.trim().to_lowercase()).collect();
        } else if let Some(d) = line.strip_prefix("Dialogue:") {
            if fmt.is_empty() {
                continue;
            }
            let parts: Vec<&str> = d.splitn(fmt.len(), ',').collect();
            let get = |n: &str| fmt.iter().position(|f| f == n).and_then(|i| parts.get(i)).copied().unwrap_or("");
            let text = tag.replace_all(get("text"), "").replace("\\N", "\n").replace("\\n", "\n");
            if text.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("{} --> {}\n{}\n\n", ts(get("start")), ts(get("end")), text.trim()));
        }
    }
    out
}

pub fn to_vtt(body: &str) -> String {
    let t = body.trim_start_matches('\u{feff}');
    if t.trim_start().starts_with("WEBVTT") {
        t.to_string()
    } else if t.contains("[Script Info]") || t.contains("Dialogue:") {
        ass_to_vtt(t)
    } else {
        srt_to_vtt(t)
    }
}

fn parse_range(h: Option<&HeaderValue>, len: u64) -> Option<(u64, u64)> {
    let s = h?.to_str().ok()?.strip_prefix("bytes=")?;
    let (a, b) = s.split(',').next()?.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (len.saturating_sub(n), len.saturating_sub(1))
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { len.saturating_sub(1) } else { b.parse::<u64>().ok()?.min(len.saturating_sub(1)) };
        (start, end)
    };
    (start <= end && start < len).then_some((start, end))
}

fn cors(mut r: Response<Body>) -> Response<Body> {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("Content-Length, Content-Range"));
    r
}

fn text_response(status: StatusCode, msg: impl Into<String>) -> Response<Body> {
    cors(
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(msg.into()))
            .unwrap(),
    )
}

fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

impl StreamInner {
    fn url(&self, kind: &str, spec: &Upstream, ext: &str) -> String {
        format!("{}/{kind}/{}{ext}", self.base.read(), spec.encode())
    }

    pub fn hls_url(&self, spec: &Upstream) -> String {
        self.url("hls", spec, ".m3u8")
    }

    pub fn file_url(&self, spec: &Upstream) -> String {
        self.url("file", spec, "")
    }

    pub fn sub_url(&self, spec: &Upstream) -> String {
        self.url("sub", spec, ".vtt")
    }

    pub fn img_url(&self, raw: &str) -> String {
        self.url("img", &Upstream::new(raw), "")
    }

    fn request(&self, spec: &Upstream, url: &str, range: Option<&str>, timeout: Duration) -> reqwest::RequestBuilder {
        let rule = self.providers.rule_for(&spec.url);
        let referer =
            spec.referer.clone().filter(|s| !s.is_empty()).or_else(|| rule.as_ref().and_then(|r| r.referer.clone()));
        let origin = spec
            .origin
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| rule.as_ref().filter(|r| r.origin_from_referer).and(referer.as_deref()).and_then(origin_of))
            .or_else(|| rule.as_ref().and_then(|r| r.origin.clone()));
        let mut pairs: Vec<(String, String)> =
            vec![("user-agent".into(), spec.ua.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| PROXY_UA.into()))];
        if let Some(r) = referer {
            pairs.push(("referer".into(), r));
        }
        if let Some(o) = origin {
            pairs.push(("origin".into(), o));
        }
        let cf = spec.cf || rule.as_ref().map(|r| r.cf_cookie).unwrap_or(false);
        let cookie = spec
            .cookie
            .clone()
            .or_else(|| cf.then(|| self.settings.read().cf_clearance.clone()).flatten())
            .filter(|c| !c.is_empty());
        if let Some(c) = cookie {
            let c = if cf && !c.contains('=') { format!("cf_clearance={}", c.trim()) } else { c };
            pairs.push(("cookie".into(), c));
        }
        if spec.scrape || rule.as_ref().map(|r| r.scrape).unwrap_or(false) {
            pairs.push(("accept".into(), "*/*".into()));
            pairs.push(("accept-language".into(), "en-US,en;q=0.9".into()));
        }
        if let Some(r) = range {
            pairs.push(("range".into(), r.to_string()));
        }
        self.http.client().get(url).headers(header_map(&pairs)).timeout(timeout)
    }

    /// GET with mirror fallback and a retry on transient errors.
    async fn fetch(&self, spec: &Upstream, range: Option<&str>, timeout: Duration) -> Result<reqwest::Response> {
        check_url(&spec.url)?;
        let mirrors = self.providers.rule_for(&spec.url).map(|r| r.mirrors).unwrap_or_default();
        let mut last = anyhow!("no candidates");
        for cand in mirror_urls(&spec.url, &mirrors) {
            for attempt in 0..3 {
                match self.request(spec, &cand, range, timeout).send().await {
                    Ok(r) if r.status().is_success() => return Ok(r),
                    Ok(r) => {
                        last = anyhow!("upstream HTTP {}", r.status().as_u16());
                        if r.status().is_server_error() && attempt < 2 {
                            tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
                            continue;
                        }
                        break;
                    }
                    Err(e) => {
                        last = anyhow!(crate::http::describe(&e));
                        if attempt < 2 && (e.is_connect() || e.is_timeout() || e.is_request()) {
                            tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
                            continue;
                        }
                        break;
                    }
                }
            }
        }
        Err(last)
    }

    /// Lightweight check that a resolved stream actually responds with media.
    pub async fn verify(&self, spec: &Upstream, hls: bool) -> Result<()> {
        if hls {
            let r = self.fetch(spec, None, Duration::from_secs(12)).await?;
            let body = r.text().await?;
            if !body.trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U") {
                bail!("not an HLS playlist");
            }
            Ok(())
        } else {
            let r = self.fetch(spec, Some("bytes=0-1023"), Duration::from_secs(12)).await?;
            let ct = r.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            if ct.starts_with("text/html") {
                bail!("upstream returned html");
            }
            Ok(())
        }
    }

    // ---------------- HLS ----------------

    async fn playlist(self: &Arc<Self>, spec: Upstream) -> Result<Arc<String>> {
        let key = sha_key(&serde_json::to_string(&spec)?);
        if let Some(p) = self.playlists_vod.get(&key).or_else(|| self.playlists_live.get(&key)) {
            return Ok(p);
        }
        let resp = self.fetch(&spec, None, Duration::from_secs(20)).await?;
        let final_url = resp.url().to_string();
        let body = resp.text().await?;
        let body = body.trim_start_matches('\u{feff}');
        if !body.trim_start().starts_with("#EXTM3U") {
            bail!("upstream did not return a playlist");
        }
        let (text, vod) = self.rewrite(body, &final_url, &spec, &key)?;
        let text = Arc::new(text);
        if vod {
            self.playlists_vod.insert(key, text.clone());
        } else {
            self.playlists_live.insert(key, text.clone());
        }
        Ok(text)
    }

    fn rewrite(self: &Arc<Self>, body: &str, final_url: &str, spec: &Upstream, job_id: &str) -> Result<(String, bool)> {
        let base = url::Url::parse(final_url)?;
        let is_master = body.contains("#EXT-X-STREAM-INF");
        let pinned;
        let body = match (is_master, spec.variant) {
            (true, Some(v)) => {
                pinned = pin_variant(body, v);
                pinned.as_deref().unwrap_or(body)
            }
            _ => body,
        };
        let abs = |u: &str| base.join(u.trim()).map(|x| x.to_string()).unwrap_or_else(|_| u.to_string());
        let uri_re = regex::Regex::new(r#"URI="([^"]+)""#).unwrap();
        let vod = body.contains("#EXT-X-ENDLIST") || body.contains("#EXT-X-PLAYLIST-TYPE:VOD");
        let mut segs: Vec<Upstream> = Vec::new();
        let mut out = String::with_capacity(body.len() * 2);
        for line in body.lines() {
            let t = line.trim();
            if t.is_empty() {
                out.push('\n');
                continue;
            }
            if t.starts_with('#') {
                let replaced = uri_re.replace_all(t, |c: &regex::Captures| {
                    let child = spec.child(abs(&c[1]));
                    let link = if is_master || t.starts_with("#EXT-X-MEDIA") {
                        self.hls_url(&child)
                    } else {
                        self.url("seg", &child, "")
                    };
                    format!("URI=\"{link}\"")
                });
                out.push_str(&replaced);
                out.push('\n');
                continue;
            }
            let child = spec.child(abs(t));
            if is_master {
                out.push_str(&self.hls_url(&child));
            } else {
                let i = segs.len();
                out.push_str(&format!("{}?j={job_id}&i={i}", self.url("seg", &child, "")));
                segs.push(child);
            }
            out.push('\n');
        }
        if !is_master && vod && !segs.is_empty() {
            self.register_job(job_id, segs);
        }
        Ok((out, vod || is_master))
    }

    fn register_job(self: &Arc<Self>, id: &str, segs: Vec<Upstream>) {
        let mut jobs = self.jobs.lock();
        if let Some(pos) = jobs.iter().position(|j| j.id == id && !j.cancelled.load(Ordering::Relaxed)) {
            let j = jobs.remove(pos);
            jobs.push(j);
            return;
        }
        let n = segs.len();
        let job = Arc::new(HlsJob {
            id: id.to_string(),
            done: (0..n).map(|_| AtomicBool::new(false)).collect(),
            claimed: (0..n).map(|_| AtomicBool::new(false)).collect(),
            segs,
            playhead: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            wake: Notify::new(),
        });
        jobs.push(job.clone());
        // Only the most recent playlists keep downloading (video + audio + one previous).
        while jobs.len() > 3 {
            let old = jobs.remove(0);
            old.cancelled.store(true, Ordering::Relaxed);
            old.wake.notify_waiters();
        }
        drop(jobs);
        let workers = self.settings.read().download_concurrency.max(1);
        for _ in 0..workers {
            let this = self.clone();
            let job = job.clone();
            tokio::spawn(async move { this.job_worker(job).await });
        }
    }

    async fn job_worker(self: Arc<Self>, job: Arc<HlsJob>) {
        loop {
            if job.cancelled.load(Ordering::Relaxed) {
                return;
            }
            let window = (!self.settings.read().download_ahead).then_some(8);
            let Some(i) = job.next(window) else {
                if job.done_count() == job.segs.len() {
                    return;
                }
                let _ = tokio::time::timeout(Duration::from_secs(3), job.wake.notified()).await;
                continue;
            };
            match self.segment(&job.segs[i]).await {
                Ok((_, size)) => {
                    job.done[i].store(true, Ordering::Relaxed);
                    job.bytes.fetch_add(size, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::debug!("prefetch segment {i} failed: {e}");
                    job.claimed[i].store(false, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    fn seg_path(&self, url: &str) -> PathBuf {
        let k = sha_key(url);
        self.seg_dir.join(&k[..2]).join(k)
    }

    /// Returns the on-disk path of a segment, downloading it once if needed.
    async fn segment(self: &Arc<Self>, spec: &Upstream) -> Result<(PathBuf, u64), String> {
        let path = self.seg_path(&spec.url);
        if let Ok(m) = tokio::fs::metadata(&path).await {
            return Ok((path, m.len()));
        }
        let fut = {
            let mut map = self.inflight.lock();
            if let Some(f) = map.get(&spec.url) {
                f.clone()
            } else {
                let this = self.clone();
                let spec2 = spec.clone();
                let key = spec.url.clone();
                let f = async move {
                    let r = this.download_segment(&spec2, &path).await.map_err(|e| format!("{e:#}"));
                    this.inflight.lock().remove(&key);
                    r.map(|n| (path, n))
                }
                .boxed()
                .shared();
                map.insert(spec.url.clone(), f.clone());
                f
            }
        };
        fut.await
    }

    async fn download_segment(&self, spec: &Upstream, path: &Path) -> Result<u64> {
        let resp = self.fetch(spec, None, Duration::from_secs(60)).await?;
        let bytes = resp.bytes().await?;
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let tmp = path.with_extension(format!("{}.tmp", rand::random::<u32>()));
        tokio::fs::write(&tmp, &bytes).await?;
        if tokio::fs::rename(&tmp, path).await.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        Ok(bytes.len() as u64)
    }

    async fn serve_segment(
        self: &Arc<Self>,
        spec: Upstream,
        q: &HashMap<String, String>,
        headers: &HeaderMap,
    ) -> Response<Body> {
        if let (Some(j), Some(i)) = (q.get("j"), q.get("i").and_then(|i| i.parse::<usize>().ok())) {
            let job = self.jobs.lock().iter().find(|x| &x.id == j).cloned();
            if let Some(job) = job {
                job.playhead.store(i, Ordering::Relaxed);
                job.wake.notify_waiters();
            }
        }
        let (path, _) = match self.segment(&spec).await {
            Ok(x) => x,
            Err(e) => return text_response(StatusCode::BAD_GATEWAY, e),
        };
        let data = match tokio::fs::read(&path).await {
            Ok(d) => d,
            Err(e) => return text_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        let p2 = path.clone();
        tokio::task::spawn_blocking(move || touch(&p2));
        let len = data.len() as u64;
        let ct = if spec.url.contains(".m4s") || spec.url.contains(".mp4") { "video/mp4" } else { "video/mp2t" };
        if let Some((s, e)) = parse_range(headers.get(header::RANGE), len) {
            let slice = Bytes::from(data).slice(s as usize..=e as usize);
            return cors(
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, ct)
                    .header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{len}"))
                    .header(header::CONTENT_LENGTH, e - s + 1)
                    .body(Body::from(slice))
                    .unwrap(),
            );
        }
        cors(
            Response::builder()
                .header(header::CONTENT_TYPE, ct)
                .header(header::CONTENT_LENGTH, len)
                .header(header::ACCEPT_RANGES, "bytes")
                .body(Body::from(data))
                .unwrap(),
        )
    }

    // ---------------- progressive files ----------------

    fn file_job(self: &Arc<Self>, spec: &Upstream) -> Arc<FileJob> {
        let key = sha_key(&spec.url);
        let mut files = self.files.lock();
        if let Some(j) = files.get(&key) {
            if !j.failed.load(Ordering::Relaxed) {
                return j.clone();
            }
        }
        let (tx, _) = watch::channel(0u64);
        let job = Arc::new(FileJob {
            part: self.file_dir.join(format!("{key}.part")),
            done_path: self.file_dir.join(format!("{key}.bin")),
            len: AtomicU64::new(0),
            water: tx,
            complete: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        files.insert(key, job.clone());
        let this = self.clone();
        let (j2, s2) = (job.clone(), spec.clone());
        tokio::spawn(async move {
            if let Err(e) = this.download_file(&j2, &s2).await {
                tracing::warn!("file download failed: {e:#}");
                j2.failed.store(true, Ordering::Relaxed);
                j2.water.send_modify(|_| {});
            }
        });
        job
    }

    async fn download_file(&self, job: &FileJob, spec: &Upstream) -> Result<()> {
        tokio::fs::create_dir_all(&self.file_dir).await?;
        let mut file = tokio::fs::File::create(&job.part).await?;
        let mut pos: u64 = 0;
        let mut retries = 0;
        loop {
            let range = format!("bytes={pos}-");
            let resp = match self.fetch(spec, Some(&range), Duration::from_secs(3600)).await {
                Ok(r) => r,
                Err(e) if retries < 5 => {
                    retries += 1;
                    tracing::debug!("file fetch retry {retries}: {e:#}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if pos > 0 && resp.status() != StatusCode::PARTIAL_CONTENT {
                bail!("upstream does not support resume");
            }
            if job.len.load(Ordering::Relaxed) == 0 {
                let total = resp
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit('/').next())
                    .and_then(|v| v.parse::<u64>().ok())
                    .or_else(|| resp.content_length());
                job.len.store(total.unwrap_or(u64::MAX), Ordering::Relaxed);
                job.water.send_modify(|_| {});
            }
            let mut stream = resp.bytes_stream();
            let mut failed = false;
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(c) => {
                        file.write_all(&c).await?;
                        pos += c.len() as u64;
                        job.water.send_replace(pos);
                    }
                    Err(e) => {
                        tracing::debug!("file stream error at {pos}: {e}");
                        failed = true;
                        break;
                    }
                }
            }
            file.flush().await?;
            let len = job.len.load(Ordering::Relaxed);
            if !failed && (len == u64::MAX || pos >= len) {
                break;
            }
            retries += 1;
            if retries > 5 {
                bail!("too many interruptions");
            }
        }
        drop(file);
        job.len.store(pos, Ordering::Relaxed);
        tokio::fs::rename(&job.part, &job.done_path).await?;
        job.complete.store(true, Ordering::Relaxed);
        job.water.send_replace(pos);
        Ok(())
    }

    async fn serve_file(self: &Arc<Self>, spec: Upstream, headers: &HeaderMap) -> Response<Body> {
        let key = sha_key(&spec.url);
        let done = self.file_dir.join(format!("{key}.bin"));
        if let Ok(m) = tokio::fs::metadata(&done).await {
            touch(&done);
            return serve_disk_range(done, m.len(), headers).await;
        }
        let job = self.file_job(&spec);
        let mut rx = job.water.subscribe();
        let deadline = Instant::now() + Duration::from_secs(20);
        while job.len.load(Ordering::Relaxed) == 0 && !job.failed.load(Ordering::Relaxed) {
            if Instant::now() > deadline || tokio::time::timeout(Duration::from_secs(20), rx.changed()).await.is_err() {
                break;
            }
        }
        let len = job.len.load(Ordering::Relaxed);
        if job.failed.load(Ordering::Relaxed) || len == 0 || len == u64::MAX {
            return self.passthrough(spec, headers).await;
        }
        let (start, end) = match headers.get(header::RANGE) {
            Some(h) => match parse_range(Some(h), len) {
                Some(r) => r,
                None => {
                    return cors(
                        Response::builder()
                            .status(StatusCode::RANGE_NOT_SATISFIABLE)
                            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                }
            },
            None => (0, len - 1),
        };
        let water = *job.water.borrow();
        if start > water + 8 * 1024 * 1024 {
            return self.passthrough(spec, headers).await;
        }
        let part = job.part.clone();
        let done_path = job.done_path.clone();
        let job2 = job.clone();
        let stream = async_stream(move |tx: tokio::sync::mpsc::Sender<std::io::Result<Bytes>>| async move {
            let open = match tokio::fs::File::open(&part).await {
                Ok(f) => Ok(f),
                Err(_) => tokio::fs::File::open(&done_path).await,
            };
            let mut f = match open {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            let mut pos = start;
            let mut rx = job2.water.subscribe();
            let mut buf = vec![0u8; 256 * 1024];
            while pos <= end {
                let avail = *rx.borrow_and_update();
                if pos >= avail {
                    if job2.failed.load(Ordering::Relaxed) {
                        let _ = tx.send(Err(std::io::Error::other("download failed"))).await;
                        return;
                    }
                    if rx.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                let want = ((avail.min(end + 1) - pos) as usize).min(buf.len());
                if f.seek(SeekFrom::Start(pos)).await.is_err() {
                    return;
                }
                match f.read(&mut buf[..want]).await {
                    Ok(0) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Ok(n) => {
                        pos += n as u64;
                        if tx.send(Ok(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });
        let status = if headers.contains_key(header::RANGE) { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
        let mut b = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "video/mp4")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, end - start + 1);
        if status == StatusCode::PARTIAL_CONTENT {
            b = b.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
        }
        cors(b.body(Body::from_stream(stream)).unwrap())
    }

    async fn passthrough(&self, spec: Upstream, headers: &HeaderMap) -> Response<Body> {
        let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
        match self.fetch(&spec, range, Duration::from_secs(3600)).await {
            Ok(r) => {
                let mut b = Response::builder().status(r.status().as_u16());
                for h in [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::CONTENT_RANGE, header::ACCEPT_RANGES] {
                    if let Some(v) = r.headers().get(&h) {
                        b = b.header(h, v.clone());
                    }
                }
                cors(b.body(Body::from_stream(r.bytes_stream())).unwrap())
            }
            Err(e) => text_response(StatusCode::BAD_GATEWAY, format!("{e:#}")),
        }
    }

    // ---------------- subtitles & images ----------------

    async fn serve_sub(&self, spec: Upstream) -> Response<Body> {
        let key = sha_key(&spec.url);
        let text = match self.subs.get(&key) {
            Some(t) => t,
            None => match self.fetch(&spec, None, Duration::from_secs(20)).await {
                Ok(r) => {
                    let t = Arc::new(to_vtt(&r.text().await.unwrap_or_default()));
                    self.subs.insert(key, t.clone());
                    t
                }
                Err(e) => return text_response(StatusCode::BAD_GATEWAY, format!("{e:#}")),
            },
        };
        cors(
            Response::builder()
                .header(header::CONTENT_TYPE, "text/vtt; charset=utf-8")
                .body(Body::from(text.to_string()))
                .unwrap(),
        )
    }

    async fn serve_img(&self, spec: Upstream) -> Response<Body> {
        let path = self.img_dir.join(sha_key(&spec.url));
        let data = match tokio::fs::read(&path).await {
            Ok(d) => d,
            Err(_) => match self.fetch(&spec, None, Duration::from_secs(20)).await {
                Ok(r) => match r.bytes().await {
                    Ok(b) => {
                        let _ = tokio::fs::create_dir_all(&self.img_dir).await;
                        let _ = tokio::fs::write(&path, &b).await;
                        b.to_vec()
                    }
                    Err(e) => return text_response(StatusCode::BAD_GATEWAY, e.to_string()),
                },
                Err(e) => return text_response(StatusCode::BAD_GATEWAY, format!("{e:#}")),
            },
        };
        let ct = match data.get(..4) {
            Some([0x89, b'P', b'N', b'G']) => "image/png",
            Some([b'G', b'I', b'F', _]) => "image/gif",
            Some([b'R', b'I', b'F', b'F']) => "image/webp",
            _ => "image/jpeg",
        };
        cors(
            Response::builder()
                .header(header::CONTENT_TYPE, ct)
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .body(Body::from(data))
                .unwrap(),
        )
    }

    pub fn downloads(&self) -> Vec<DownloadStatus> {
        self.jobs
            .lock()
            .iter()
            .map(|j| DownloadStatus {
                id: j.id.clone(),
                total: j.segs.len(),
                done: j.done_count(),
                playhead: j.playhead.load(Ordering::Relaxed),
                bytes: j.bytes.load(Ordering::Relaxed),
                active: !j.cancelled.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Stops background downloads (e.g. when the player closes).
    pub fn stop_downloads(&self) {
        for j in self.jobs.lock().drain(..) {
            j.cancelled.store(true, Ordering::Relaxed);
            j.wake.notify_waiters();
        }
    }

    /// Paths that must not be deleted by the janitor right now.
    pub fn busy_files(&self) -> Vec<PathBuf> {
        self.files
            .lock()
            .values()
            .filter(|j| !j.complete.load(Ordering::Relaxed) && !j.failed.load(Ordering::Relaxed))
            .map(|j| j.part.clone())
            .collect()
    }

    pub fn clear_memory(&self) {
        self.playlists_vod.invalidate_all();
        self.playlists_live.invalidate_all();
        self.subs.invalidate_all();
        self.files.lock().retain(|_, j| !j.complete.load(Ordering::Relaxed) && !j.failed.load(Ordering::Relaxed));
    }
}

/// Minimal channel-backed body stream.
fn async_stream<F, Fut>(f: F) -> impl futures::Stream<Item = std::io::Result<Bytes>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<std::io::Result<Bytes>>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(f(tx));
    tokio_stream_from(rx)
}

fn tokio_stream_from(
    mut rx: tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

async fn serve_disk_range(path: PathBuf, len: u64, headers: &HeaderMap) -> Response<Body> {
    let (start, end, status) = match parse_range(headers.get(header::RANGE), len) {
        Some((s, e)) => (s, e, StatusCode::PARTIAL_CONTENT),
        None => (0, len.saturating_sub(1), StatusCode::OK),
    };
    let stream = async_stream(move |tx| async move {
        let Ok(mut f) = tokio::fs::File::open(&path).await else { return };
        if f.seek(SeekFrom::Start(start)).await.is_err() {
            return;
        }
        let mut left = end + 1 - start;
        let mut buf = vec![0u8; 256 * 1024];
        while left > 0 {
            let n = match f.read(&mut buf[..(left as usize).min(256 * 1024)]).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            left -= n as u64;
            if tx.send(Ok(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                return;
            }
        }
    });
    let mut b = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, end + 1 - start);
    if status == StatusCode::PARTIAL_CONTENT {
        b = b.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
    }
    cors(b.body(Body::from_stream(stream)).unwrap())
}

async fn handle(
    State(inner): State<Arc<StreamInner>>,
    AxPath((token, kind, data)): AxPath<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response<Body> {
    if token != inner.token {
        return text_response(StatusCode::FORBIDDEN, "bad token");
    }
    let Some(spec) = Upstream::decode(&data) else {
        return text_response(StatusCode::BAD_REQUEST, "bad resource");
    };
    if let Err(e) = check_url(&spec.url) {
        return text_response(StatusCode::BAD_REQUEST, e.to_string());
    }
    match kind.as_str() {
        "hls" => match inner.playlist(spec).await {
            Ok(text) => cors(
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .body(Body::from(text.to_string()))
                    .unwrap(),
            ),
            Err(e) => text_response(StatusCode::BAD_GATEWAY, format!("{e:#}")),
        },
        "seg" => inner.serve_segment(spec, &q, &headers).await,
        "file" => inner.serve_file(spec, &headers).await,
        "sub" => inner.serve_sub(spec).await,
        "img" => inner.serve_img(spec).await,
        _ => text_response(StatusCode::NOT_FOUND, "unknown"),
    }
}

async fn preflight() -> Response<Body> {
    cors(
        Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, OPTIONS")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "*")
            .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
            .body(Body::empty())
            .unwrap(),
    )
}

impl StreamServer {
    pub async fn start(
        cache_dir: &Path,
        http: Http,
        providers: Arc<Providers>,
        settings: Arc<RwLock<Settings>>,
    ) -> Result<Self> {
        let token: String = (0..24).map(|_| format!("{:x}", rand::random::<u8>() % 16)).collect();
        let inner = Arc::new(StreamInner {
            http,
            providers,
            settings,
            seg_dir: cache_dir.join("segments"),
            file_dir: cache_dir.join("files"),
            img_dir: cache_dir.join("images"),
            token: token.clone(),
            base: RwLock::new(String::new()),
            playlists_vod: moka::sync::Cache::builder()
                .max_capacity(500)
                .time_to_live(Duration::from_secs(1800))
                .build(),
            playlists_live: moka::sync::Cache::builder().max_capacity(200).time_to_live(Duration::from_secs(3)).build(),
            subs: moka::sync::Cache::builder().max_capacity(200).time_to_live(Duration::from_secs(3600)).build(),
            inflight: Mutex::new(HashMap::new()),
            jobs: Mutex::new(Vec::new()),
            files: Mutex::new(HashMap::new()),
        });
        for d in [&inner.seg_dir, &inner.file_dir, &inner.img_dir] {
            tokio::fs::create_dir_all(d).await?;
        }
        // Leftover partial downloads from a previous run cannot be resumed.
        if let Ok(mut rd) = tokio::fs::read_dir(&inner.file_dir).await {
            while let Ok(Some(e)) = rd.next_entry().await {
                if e.file_name().to_string_lossy().ends_with(".part") {
                    let _ = tokio::fs::remove_file(e.path()).await;
                }
            }
        }
        let listener = tokio::net::TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0)).await?;
        let addr = listener.local_addr()?;
        *inner.base.write() = format!("http://127.0.0.1:{}/s/{token}", addr.port());
        let app =
            Router::new().route("/s/{token}/{kind}/{data}", get(handle).options(preflight)).with_state(inner.clone());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("stream server stopped: {e}");
            }
        });
        tracing::info!("stream server on {addr}");
        Ok(Self { inner, addr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_variant() {
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\na.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2\nb.m3u8\n";
        let p = pin_variant(m, 1).unwrap();
        assert!(p.contains("b.m3u8") && !p.contains("a.m3u8"));
        assert!(pin_variant(m, 5).is_none());
    }

    #[test]
    fn upstream_roundtrip() {
        let u = Upstream {
            url: "https://x/y.m3u8".into(),
            referer: Some("https://r/".into()),
            variant: Some(2),
            ..Default::default()
        };
        assert_eq!(Upstream::decode(&format!("{}.m3u8", u.encode())).unwrap(), u);
    }

    #[test]
    fn ranges() {
        let h = HeaderValue::from_static("bytes=10-");
        assert_eq!(parse_range(Some(&h), 100), Some((10, 99)));
        let h = HeaderValue::from_static("bytes=-10");
        assert_eq!(parse_range(Some(&h), 100), Some((90, 99)));
        let h = HeaderValue::from_static("bytes=200-");
        assert_eq!(parse_range(Some(&h), 100), None);
    }

    #[test]
    fn subtitles() {
        assert!(to_vtt("1\n00:00:01,000 --> 00:00:02,000\nhi").contains("00:00:01.000"));
        let ass = "[Script Info]\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,{\\i1}Hello\\Nworld";
        let v = to_vtt(ass);
        assert!(v.contains("00:00:01.00") && v.contains("Hello\nworld"), "{v}");
    }

    #[test]
    fn blocks_local() {
        assert!(check_url("http://127.0.0.1/x").is_err());
        assert!(check_url("http://192.168.1.2/x").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("https://cdn.example.com/x").is_ok());
    }
}
