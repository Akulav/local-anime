//! Shared outbound HTTP client used by providers, metadata and the stream proxy.

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{redirect, Client, Method};
use serde::{Deserialize, Serialize};

pub const GENERIC_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
pub const PROXY_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:126.0) Gecko/20100101 Firefox/126.0";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct Http {
    follow: Client,
    manual: Client,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsRequest {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub redirect: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct JsResponseMeta {
    pub status: u16,
    pub url: String,
    pub redirected: bool,
    pub headers: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn build(policy: redirect::Policy) -> Client {
    Client::builder()
        .redirect(policy)
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(16)
        .tcp_nodelay(true)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .zstd(true)
        .build()
        .expect("http client")
}

/// Headers a desktop browser sends; used for "scrape" requests where sites gate on them.
fn browser_headers(h: &mut HeaderMap) {
    let defaults: [(&str, &str); 6] = [
        ("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        ("accept-language", "en-US,en;q=0.9"),
        ("sec-ch-ua", "\"Chromium\";v=\"124\", \"Google Chrome\";v=\"124\", \"Not-A.Brand\";v=\"99\""),
        ("sec-ch-ua-mobile", "?0"),
        ("sec-ch-ua-platform", "\"Windows\""),
        ("upgrade-insecure-requests", "1"),
    ];
    for (k, v) in defaults {
        if !h.contains_key(k) {
            h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
    }
}

pub fn header_map(pairs: &[(String, String)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.trim().as_bytes()), HeaderValue::from_str(v.trim())) {
            // Body-framing headers are computed by the client.
            if name == reqwest::header::CONTENT_LENGTH || name == reqwest::header::HOST {
                continue;
            }
            h.append(name, val);
        }
    }
    h
}

impl Http {
    pub fn new() -> Self {
        Self { follow: build(redirect::Policy::limited(10)), manual: build(redirect::Policy::none()) }
    }

    pub fn client(&self) -> &Client {
        &self.follow
    }

    /// Executes a request issued by provider JavaScript and returns metadata plus the decoded body.
    pub async fn js_request(&self, req: JsRequest) -> (JsResponseMeta, String) {
        match self.js_request_inner(&req).await {
            Ok(r) => r,
            Err(e) => (
                JsResponseMeta {
                    status: 0,
                    url: req.url.clone(),
                    redirected: false,
                    headers: vec![],
                    error: Some(format!("{e:#}")),
                },
                String::new(),
            ),
        }
    }

    async fn js_request_inner(&self, req: &JsRequest) -> Result<(JsResponseMeta, String)> {
        let method = Method::from_bytes(req.method.as_deref().unwrap_or("GET").as_bytes())?;
        let client = if req.redirect.as_deref() == Some("manual") || req.redirect.as_deref() == Some("error") {
            &self.manual
        } else {
            &self.follow
        };
        let mut headers = header_map(&req.headers);
        if !headers.contains_key(reqwest::header::USER_AGENT) {
            headers.insert(reqwest::header::USER_AGENT, HeaderValue::from_static(GENERIC_UA));
        }
        if req.profile.as_deref() == Some("browser") {
            browser_headers(&mut headers);
        }
        let timeout = req.timeout_ms.map(Duration::from_millis).unwrap_or(DEFAULT_TIMEOUT);
        let mut rb = client.request(method, &req.url).headers(headers).timeout(timeout);
        if let Some(body) = &req.body {
            rb = rb.body(body.clone());
        }
        let resp = rb.send().await.map_err(|e| anyhow!(describe(&e)))?;
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        let bytes = resp.bytes().await.map_err(|e| anyhow!(describe(&e)))?;
        Ok((
            JsResponseMeta { status, redirected: final_url != req.url, url: final_url, headers, error: None },
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    }
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

pub fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        format!("connect error: {e}")
    } else {
        e.to_string()
    }
}
