//! AniList GraphQL client: rate limited, request-coalesced and cached (memory + SQLite).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures::future::{BoxFuture, FutureExt, Shared};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::http::Http;
use crate::store::Store;

const API: &str = "https://graphql.anilist.co";
const BURST: f64 = 8.0;

type InflightRequest = Shared<BoxFuture<'static, Option<Arc<Value>>>>;

pub struct Anilist {
    http: Http,
    store: Arc<Store>,
    memory: moka::sync::Cache<String, Arc<Value>>,
    bucket: Mutex<Bucket>,
    inflight: parking_lot::Mutex<HashMap<String, InflightRequest>>,
}

struct Bucket {
    tokens: f64,
    refill_every: Duration,
    last: Instant,
    cooldown_until: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MediaTitles {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub romaji: Option<String>,
    pub english: Option<String>,
    pub native: Option<String>,
    pub synonyms: Vec<String>,
    pub episodes: Option<i64>,
    pub aired_episodes: Option<i64>,
    pub status: Option<String>,
    pub format: Option<String>,
    pub cover: Option<String>,
    pub is_adult: bool,
}

impl MediaTitles {
    pub fn display(&self) -> String {
        self.english.clone().or_else(|| self.romaji.clone()).unwrap_or_else(|| format!("#{}", self.id))
    }

    /// Number of episodes that should exist right now.
    pub fn expected_episodes(&self) -> Option<i64> {
        self.aired_episodes.or(self.episodes)
    }

    pub fn from_media(m: &Value) -> Self {
        let s = |v: &Value| v.as_str().map(str::to_string).filter(|x| !x.trim().is_empty());
        let next = m["nextAiringEpisode"]["episode"].as_i64();
        Self {
            id: m["id"].as_i64().unwrap_or(0),
            id_mal: m["idMal"].as_i64(),
            romaji: s(&m["title"]["romaji"]),
            english: s(&m["title"]["english"]),
            native: s(&m["title"]["native"]),
            synonyms: m["synonyms"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            episodes: m["episodes"].as_i64(),
            aired_episodes: next.map(|n| n - 1).filter(|n| *n > 0),
            status: s(&m["status"]),
            format: s(&m["format"]),
            cover: s(&m["coverImage"]["extraLarge"]).or_else(|| s(&m["coverImage"]["large"])),
            is_adult: m["isAdult"].as_bool().unwrap_or(false),
        }
    }
}

const CARD_FIELDS: &str = r#"
  id idMal title { romaji english native } synonyms
  coverImage { extraLarge large color } bannerImage
  format status episodes duration season seasonYear averageScore popularity isAdult
  genres nextAiringEpisode { episode airingAt timeUntilAiring }
"#;

impl Anilist {
    pub fn new(http: Http, store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            http,
            store,
            memory: moka::sync::Cache::builder().max_capacity(2_000).time_to_live(Duration::from_secs(3600)).build(),
            bucket: Mutex::new(Bucket {
                tokens: BURST,
                refill_every: Duration::from_millis(1500),
                last: Instant::now(),
                cooldown_until: None,
            }),
            inflight: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    async fn wait_slot(&self) {
        loop {
            let sleep_for = {
                let mut b = self.bucket.lock().await;
                let now = Instant::now();
                match b.cooldown_until.filter(|u| *u > now) {
                    Some(until) => until - now,
                    None => {
                        b.cooldown_until = None;
                        let periods = (now.duration_since(b.last).as_secs_f64() / b.refill_every.as_secs_f64()).floor();
                        if periods >= 1.0 {
                            b.tokens = (b.tokens + periods).min(BURST);
                            b.last = now;
                        }
                        if b.tokens >= 1.0 {
                            b.tokens -= 1.0;
                            return;
                        }
                        b.refill_every
                    }
                }
            };
            tokio::time::sleep(sleep_for).await;
        }
    }

    /// Raw GraphQL request returning `{ data, errors }`. Cached for `ttl` seconds.
    pub async fn request(self: &Arc<Self>, query: &str, vars: &Value, ttl: i64) -> Option<Arc<Value>> {
        let key = format!("al:{}", blake_key(query, vars));
        if let Some(v) = self.memory.get(&key) {
            return Some(v);
        }
        if let Some(raw) = self.store.kv_get(&key) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                let v = Arc::new(v);
                self.memory.insert(key, v.clone());
                return Some(v);
            }
        }
        let fut = {
            let mut inflight = self.inflight.lock();
            if let Some(f) = inflight.get(&key) {
                f.clone()
            } else {
                let this = self.clone();
                let q = query.to_string();
                let v = vars.clone();
                let k = key.clone();
                let f = async move {
                    let out = this.perform(&q, &v, 0).await.ok().map(Arc::new);
                    if let Some(val) = &out {
                        if val.get("data").map(|d| !d.is_null()).unwrap_or(false) {
                            this.memory.insert(k.clone(), val.clone());
                            this.store.kv_set(&k, &val.to_string(), ttl);
                        }
                    }
                    this.inflight.lock().remove(&k);
                    out
                }
                .boxed()
                .shared();
                inflight.insert(key.clone(), f.clone());
                f
            }
        };
        fut.await
    }

    async fn perform(&self, query: &str, vars: &Value, attempt: u32) -> Result<Value> {
        self.wait_slot().await;
        let resp = self
            .http
            .client()
            .post(API)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .timeout(Duration::from_secs(20))
            .body(json!({ "query": query, "variables": vars }).to_string())
            .send()
            .await?;
        let status = resp.status().as_u16();
        let hdr = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        if let Some(limit) = hdr("x-ratelimit-limit").filter(|l| *l > 0) {
            let safe = (limit as f64 * 0.8).max(1.0);
            self.bucket.lock().await.refill_every =
                Duration::from_millis((60_000.0 / safe).clamp(300.0, 60_000.0) as u64);
        }
        if status == 429 {
            let retry = hdr("retry-after").unwrap_or(5).min(60);
            self.bucket.lock().await.cooldown_until = Some(Instant::now() + Duration::from_secs(retry));
            if attempt >= 4 {
                return Err(anyhow!("AniList rate limited"));
            }
            return Box::pin(self.perform(query, vars, attempt + 1)).await;
        }
        let v: Value = resp.json().await?;
        Ok(v)
    }

    async fn data(self: &Arc<Self>, query: &str, vars: Value, ttl: i64) -> Result<Value> {
        let r = self.request(query, &vars, ttl).await.ok_or_else(|| anyhow!("AniList unavailable"))?;
        match r.get("data") {
            Some(d) if !d.is_null() => Ok(d.clone()),
            _ => Err(anyhow!("AniList error: {}", r["errors"][0]["message"].as_str().unwrap_or("unknown"))),
        }
    }

    pub async fn search(self: &Arc<Self>, q: &str, page: u32, adult: bool) -> Result<Value> {
        let query = format!(
            "query ($search: String, $page: Int, $adult: Boolean) {{ Page(page: $page, perPage: 30) {{
               pageInfo {{ hasNextPage }}
               media(search: $search, type: ANIME, isAdult: $adult, sort: [SEARCH_MATCH, POPULARITY_DESC]) {{ {CARD_FIELDS} }} }} }}"
        );
        let mut vars = json!({ "search": q, "page": page });
        if !adult {
            vars["adult"] = json!(false);
        }
        self.data(&query, vars, 3600).await.map(|d| d["Page"].clone())
    }

    pub async fn home(self: &Arc<Self>) -> Result<Value> {
        let (season, year) = current_season();
        let query = format!(
            "query ($season: MediaSeason, $year: Int) {{
               trending: Page(perPage: 24) {{ media(type: ANIME, sort: TRENDING_DESC, isAdult: false) {{ {CARD_FIELDS} }} }}
               season: Page(perPage: 24) {{ media(type: ANIME, season: $season, seasonYear: $year, sort: POPULARITY_DESC, isAdult: false) {{ {CARD_FIELDS} }} }}
               top: Page(perPage: 24) {{ media(type: ANIME, sort: SCORE_DESC, isAdult: false) {{ {CARD_FIELDS} }} }}
             }}"
        );
        self.data(&query, json!({ "season": season, "year": year }), 3 * 3600).await
    }

    pub async fn details(self: &Arc<Self>, id: i64) -> Result<Value> {
        let query = format!(
            "query ($id: Int) {{ Media(id: $id, type: ANIME) {{ {CARD_FIELDS}
               description(asHtml: false) studios(isMain: true) {{ nodes {{ name }} }}
               trailer {{ id site }}
               streamingEpisodes {{ title thumbnail }}
               relations {{ edges {{ relationType node {{ id type format title {{ romaji english }} coverImage {{ large }} }} }} }}
               recommendations(perPage: 12, sort: RATING_DESC) {{ nodes {{ mediaRecommendation {{ {CARD_FIELDS} }} }} }}
             }} }}"
        );
        self.data(&query, json!({ "id": id }), 3600).await.map(|d| d["Media"].clone())
    }

    pub async fn titles(self: &Arc<Self>, id: i64) -> Result<MediaTitles> {
        Ok(MediaTitles::from_media(&self.details(id).await?))
    }
}

fn blake_key(query: &str, vars: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(query.as_bytes());
    h.update(vars.to_string().as_bytes());
    hex::encode(&h.finalize()[..16])
}

fn current_season() -> (&'static str, i32) {
    let secs = crate::store::now_secs();
    // days since epoch -> civil date (Howard Hinnant's algorithm)
    let z = secs / 86_400 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as i32;
    let season = match month {
        1..=3 => "WINTER",
        4..=6 => "SPRING",
        7..=9 => "SUMMER",
        _ => "FALL",
    };
    (season, year)
}
