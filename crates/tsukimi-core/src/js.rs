//! Embedded QuickJS runtime that executes the original remote provider modules.
//!
//! Every provider gets its own runtime (isolated globals, parallel CPU work). Network,
//! timers, caching, crypto and URL parsing are implemented in Rust and exposed to the
//! module through the `__host` object; `js/prelude.js` builds the Node-like environment
//! and the `ctx` API on top of it.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use base64::Engine;
use rquickjs::function::{Async, This};
use rquickjs::{async_with, AsyncContext, AsyncRuntime, Ctx, Exception, Function, Module, Object, Promise};
use serde::Deserialize;
use serde_json::Value;

use crate::anilist::Anilist;
use crate::http::{Http, JsRequest};

const VENDOR: &str = include_str!("../js/vendor.js");
const PRELUDE: &str = include_str!("../js/prelude.js");

pub struct HostServices {
    pub http: Http,
    pub anilist: Arc<Anilist>,
    cache: moka::sync::Cache<String, (Arc<str>, Instant)>,
}

static HOST: OnceLock<Arc<HostServices>> = OnceLock::new();

pub fn init_host(http: Http, anilist: Arc<Anilist>) {
    let _ = HOST.set(Arc::new(HostServices {
        http,
        anilist,
        cache: moka::sync::Cache::builder().max_capacity(20_000).time_to_live(Duration::from_secs(24 * 3600)).build(),
    }));
}

fn host() -> &'static Arc<HostServices> {
    HOST.get().expect("js host services not initialised")
}

/// Drops all provider-level cache entries (used by "clear cache").
pub fn clear_provider_cache() {
    if let Some(h) = HOST.get() {
        h.cache.invalidate_all();
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCaps {
    pub name: String,
    pub resolve_show_id: bool,
    pub is_direct_id: bool,
    pub browse: bool,
}

/// A loaded provider module.
///
/// The QuickJS runtime and context are intentionally never freed: tearing a runtime down
/// while host futures or timers are still queued corrupts the heap. Providers live for the
/// whole process; a registry update leaks the superseded instance (a few MB at most).
pub struct JsProvider {
    pub id: String,
    pub caps: ProviderCaps,
    ctx: std::mem::ManuallyDrop<AsyncContext>,
    _rt: std::mem::ManuallyDrop<AsyncRuntime>,
    /// One call at a time per provider. Two `async_with!` calls interleaving on the same context
    /// (rquickjs 0.9, `parallel`) corrupt the QuickJS heap: two concurrent `resolveShowId` calls on
    /// AnimeYa crashed the app with STATUS_HEAP_CORRUPTION within seconds (examples/stress_provider).
    calls: tokio::sync::Mutex<()>,
}

fn js_error(ctx: &Ctx<'_>, err: rquickjs::Error) -> String {
    if let rquickjs::Error::Exception = err {
        let v = ctx.catch();
        if let Some(ex) = v.clone().into_object().and_then(Exception::from_object) {
            return ex.message().unwrap_or_else(|| "exception".into());
        }
        if let Some(s) = v.as_string().and_then(|s| s.to_string().ok()) {
            return s;
        }
        return format!("{v:?}");
    }
    err.to_string()
}

fn url_json(u: &url::Url) -> String {
    let port = u.port().map(|p| p.to_string()).unwrap_or_default();
    let hostname = u.host_str().unwrap_or("").to_string();
    let host = if port.is_empty() { hostname.clone() } else { format!("{hostname}:{port}") };
    serde_json::json!({
        "href": u.as_str(),
        "origin": u.origin().ascii_serialization(),
        "protocol": format!("{}:", u.scheme()),
        "username": u.username(),
        "password": u.password().unwrap_or(""),
        "host": host,
        "hostname": hostname,
        "port": port,
        "pathname": u.path(),
        "search": u.query().filter(|q| !q.is_empty()).map(|q| format!("?{q}")).unwrap_or_default(),
        "hash": u.fragment().filter(|f| !f.is_empty()).map(|f| format!("#{f}")).unwrap_or_default(),
    })
    .to_string()
}

fn url_set(href: &str, field: &str, value: &str) -> Option<String> {
    let mut u = url::Url::parse(href).ok()?;
    match field {
        "protocol" => {
            let _ = u.set_scheme(value.trim_end_matches(':'));
        }
        "username" => {
            let _ = u.set_username(value);
        }
        "password" => {
            let _ = u.set_password(Some(value).filter(|v| !v.is_empty()));
        }
        "host" => {
            let (h, p) = match value.rsplit_once(':') {
                Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, p.parse::<u16>().ok()),
                _ => (value, None),
            };
            if u.set_host(Some(h)).is_ok() {
                let _ = u.set_port(p);
            }
        }
        "hostname" => {
            let _ = u.set_host(Some(value));
        }
        "port" => {
            let _ = u.set_port(value.parse::<u16>().ok());
        }
        "pathname" => u.set_path(value),
        "search" => {
            let q = value.trim_start_matches('?');
            u.set_query(Some(q).filter(|q| !q.is_empty()));
        }
        "hash" => {
            let f = value.trim_start_matches('#');
            u.set_fragment(Some(f).filter(|f| !f.is_empty()));
        }
        _ => return None,
    }
    Some(url_json(&u))
}

/// Decodes base64 or base64url, padded or not, like Node's `Buffer.from(s, 'base64url')`.
fn lenient_b64(s: &str) -> Option<Vec<u8>> {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    const ENGINE: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let normalized: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    ENGINE.decode(normalized).ok()
}

fn aes_cbc_decrypt(b64url: &str, key: &str, iv: &str) -> Option<String> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    let data = lenient_b64(b64url)?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let mut k = [0u8; 32];
    let kb = key.as_bytes();
    k[..kb.len().min(32)].copy_from_slice(&kb[..kb.len().min(32)]);
    let ivb: [u8; 16] = iv.as_bytes().try_into().ok()?;
    let mut buf = data;
    let out = cbc::Decryptor::<aes::Aes256>::new(&k.into(), &ivb.into()).decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?;
    String::from_utf8(out.to_vec()).ok()
}

fn hmac_sha256_b64url(key: &str, msg: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn install_host<'js>(ctx: &Ctx<'js>, provider_id: &str) -> rquickjs::Result<()> {
    let h = Object::new(ctx.clone())?;

    h.set(
        "http",
        Function::new(
            ctx.clone(),
            Async(|req: String| async move {
                let parsed: std::result::Result<JsRequest, _> = serde_json::from_str(&req);
                let (meta, body) = match parsed {
                    Ok(r) => {
                        let http = host().http.clone();
                        // Run on the tokio pool so the transfer progresses independently of the JS lock.
                        match tokio::spawn(async move { http.js_request(r).await }).await {
                            Ok(x) => x,
                            Err(e) => return Ok::<Vec<String>, rquickjs::Error>(vec![
                                serde_json::json!({ "status": 0, "url": "", "headers": [], "error": e.to_string() }).to_string(),
                                String::new(),
                            ]),
                        }
                    }
                    Err(e) => {
                        return Ok(vec![
                            serde_json::json!({ "status": 0, "url": "", "headers": [], "error": format!("bad request: {e}") })
                                .to_string(),
                            String::new(),
                        ])
                    }
                };
                Ok(vec![serde_json::to_string(&meta).unwrap_or_default(), body])
            }),
        )?,
    )?;

    h.set(
        "sleep",
        Function::new(
            ctx.clone(),
            Async(|ms: f64| async move {
                tokio::time::sleep(Duration::from_millis(ms.clamp(0.0, 600_000.0) as u64)).await;
                Ok::<(), rquickjs::Error>(())
            }),
        )?,
    )?;

    h.set(
        "anilist",
        Function::new(
            ctx.clone(),
            Async(|query: String, vars: String| async move {
                let al = host().anilist.clone();
                let vars: Value = serde_json::from_str(&vars).unwrap_or(Value::Null);
                let r = tokio::spawn(async move { al.request(&query, &vars, 3600).await }).await.ok().flatten();
                Ok::<Option<String>, rquickjs::Error>(r.map(|v| v.to_string()))
            }),
        )?,
    )?;

    let pid = provider_id.to_string();
    h.set(
        "log",
        Function::new(ctx.clone(), move |level: String, msg: String| {
            let msg: String = msg.chars().take(2000).collect();
            match level.as_str() {
                "error" => tracing::warn!(provider = %pid, "{msg}"),
                "warn" => tracing::info!(provider = %pid, "{msg}"),
                "info" => tracing::debug!(provider = %pid, "{msg}"),
                _ => tracing::trace!(provider = %pid, "{msg}"),
            }
        })?,
    )?;

    h.set(
        "cacheGet",
        Function::new(ctx.clone(), |key: String| -> Option<String> {
            let c = &host().cache;
            match c.get(&key) {
                Some((v, exp)) if exp > Instant::now() => Some(v.to_string()),
                Some(_) => {
                    c.invalidate(&key);
                    None
                }
                None => None,
            }
        })?,
    )?;

    h.set(
        "cacheSet",
        Function::new(ctx.clone(), |key: String, value: String, ttl: f64| {
            let ttl = if ttl > 0.0 { ttl.min(7.0 * 86_400.0) } else { 600.0 };
            host().cache.insert(key, (Arc::from(value), Instant::now() + Duration::from_secs_f64(ttl)));
        })?,
    )?;

    h.set(
        "urlParse",
        Function::new(ctx.clone(), |input: String, base: Option<String>| -> Option<String> {
            let parsed = match base {
                Some(b) => url::Url::parse(&b).ok().and_then(|b| b.join(&input).ok()),
                None => url::Url::parse(&input).ok(),
            };
            parsed.map(|u| url_json(&u))
        })?,
    )?;

    h.set(
        "urlSet",
        Function::new(ctx.clone(), |href: String, field: String, value: String| url_set(&href, &field, &value))?,
    )?;

    h.set(
        "aesCbcDecrypt",
        Function::new(ctx.clone(), |data: String, key: String, iv: String| aes_cbc_decrypt(&data, &key, &iv))?,
    )?;

    h.set("hmacSha256", Function::new(ctx.clone(), |key: String, msg: String| hmac_sha256_b64url(&key, &msg))?)?;

    ctx.globals().set("__host", h)?;
    Ok(())
}

/// Loads run one at a time: creating QuickJS runtimes and evaluating modules on several threads
/// at once crashed the live provider test with STATUS_HEAP_CORRUPTION. (The app loads with
/// `join_all` on one task, so it never did this; the lock makes it safe for every caller.)
static LOADING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl JsProvider {
    pub async fn load(id: &str, source: String) -> Result<Self> {
        let _one_at_a_time = LOADING.lock().await;
        let rt = AsyncRuntime::new().context("quickjs runtime")?;
        rt.set_memory_limit(384 * 1024 * 1024).await;
        // Below the 2 MiB of a tokio worker thread, so deep recursion in a provider becomes a
        // catchable JS "stack overflow" instead of overflowing the real stack (which kills the
        // process silently). rquickjs measures it from where each call enters the runtime.
        rt.set_max_stack_size(1024 * 1024).await;
        rt.set_gc_threshold(32 * 1024 * 1024).await;
        let ctx = AsyncContext::full(&rt).await.context("quickjs context")?;

        let pid = id.to_string();
        ctx.with(|ctx| -> std::result::Result<(), String> {
            install_host(&ctx, &pid).map_err(|e| js_error(&ctx, e))?;
            ctx.eval::<(), _>(VENDOR).map_err(|e| format!("vendor: {}", js_error(&ctx, e)))?;
            ctx.eval::<(), _>(PRELUDE).map_err(|e| format!("prelude: {}", js_error(&ctx, e)))?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow!(e))?;

        // Keep timers and host futures moving even when no call is awaiting.
        tokio::spawn(rt.drive());

        let module_name = format!("{id}.mjs");
        let pid = id.to_string();
        let init = async_with!(ctx => |ctx| {
            let r: rquickjs::Result<String> = async {
                let (module, promise) = Module::declare(ctx.clone(), module_name, source)?.eval()?;
                promise.into_future::<()>().await?;
                let ns: Object = module.namespace()?;
                let d: Object = ctx.globals().get("__dango")?;
                let f: Function = d.get("init")?;
                let p: Promise = f.call((This(d.clone()), pid, ns))?;
                p.into_future::<String>().await
            }
            .await;
            r.map_err(|e| js_error(&ctx, e))
        });
        let caps_json = init.await.map_err(|e| anyhow!(e))?;
        let caps: ProviderCaps = serde_json::from_str(&caps_json)?;
        Ok(Self {
            id: id.to_string(),
            caps,
            ctx: std::mem::ManuallyDrop::new(ctx),
            _rt: std::mem::ManuallyDrop::new(rt),
            calls: tokio::sync::Mutex::new(()),
        })
    }

    /// Invokes `provider[method](...args)` and returns its JSON result.
    ///
    /// The timeout is enforced inside JS (a race against a host timer) so the Rust future
    /// always runs to completion under the runtime lock; dropping it mid-flight is unsound.
    /// Calls to the same provider run one after another (see `calls`); waiting for the turn is
    /// outside `async_with!`, so dropping a waiting call is fine.
    pub async fn call(
        &self,
        method: &str,
        args: Value,
        request_ctx: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let _turn = self.calls.lock().await;
        let m = method.to_string();
        let a = args.to_string();
        let rc = request_ctx.map(|v| v.to_string()).unwrap_or_default();
        let ms = timeout.as_millis() as f64;
        let s = async_with!(self.ctx => |ctx| {
            let r: rquickjs::Result<String> = async {
                let d: Object = ctx.globals().get("__dango")?;
                let f: Function = d.get("call")?;
                let p: Promise = f.call((This(d.clone()), m, a, rc, ms))?;
                p.into_future::<String>().await
            }
            .await;
            r.map_err(|e| js_error(&ctx, e))
        })
        .await
        .map_err(|e| anyhow!(e))?;
        Ok(serde_json::from_str(&s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_roundtrip() {
        let j: Value =
            serde_json::from_str(&url_json(&url::Url::parse("https://a.b:8443/x/y?q=1#h").unwrap())).unwrap();
        assert_eq!(j["host"], "a.b:8443");
        assert_eq!(j["search"], "?q=1");
        let s: Value = serde_json::from_str(&url_set("https://a.b/x", "search", "?z=2").unwrap()).unwrap();
        assert_eq!(s["href"], "https://a.b/x?z=2");
    }

    #[test]
    fn hmac_is_base64url() {
        assert!(!hmac_sha256_b64url("k", "m").contains('='));
    }
}
