//! Exercises the embedded runtime with a synthetic provider (offline) and,
//! when TSUKIMI_LIVE=1, with real modules from the provider registry.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tsukimi_core::{anilist::Anilist, http::Http, js, store::Store};

fn init() {
    let http = Http::new();
    let dir = std::env::temp_dir().join(format!("tsukimi-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(&dir.join("t.db")).unwrap());
    js::init_host(http.clone(), Anilist::new(http, store));
}

const SYNTHETIC: &str = r#"
export const GENRES = ['a', 'b']
export default function createProvider(ctx) {
  return {
    name: 'Synthetic',
    async search({ query }) {
      const u = new URL('https://example.com/a/b?x=1')
      u.searchParams.set('q', query)
      const $ = ctx.cheerio.load('<ul><li class="s" data-id="7">One</li><li class="s" data-id="9">Two</li></ul>')
      const items = $('li.s').map((_, el) => ({ id: $(el).attr('data-id'), name: $(el).text() })).get()
      ctx.cache.set('k', { v: 1 }, 60)
      await new Promise((r) => setTimeout(r, 5))
      return {
        href: u.href,
        items,
        cached: ctx.cache.get('k'),
        b64: Buffer.from('héllo').toString('base64'),
        atob: atob(btoa('xyz')),
        match: ctx.titleMatch.pickBestMatch([{ title: 'Shingeki no Kyojin' }, { title: 'Naruto' }], ['Shingeki no Kyojin Season 2'])?.item.title,
        evald: eval('1 + 2'),
        uni: 'Ünïcode café'.replace(/[^\p{L}\s]/gu, ''),
      }
    },
    async getEpisodes() { return { episodes: ['1', '2'], description: '' } },
    async getStreamUrls() { throw new Error('boom') },
    hang() { return new Promise(() => {}) },
  }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn synthetic_provider_runs() {
    init();
    let p = js::JsProvider::load("synthetic", SYNTHETIC.to_string()).await.expect("load");
    assert_eq!(p.caps.name, "Synthetic");
    let r = p.call("search", json!([{ "query": "naruto" }]), None, Duration::from_secs(10)).await.expect("search");
    assert_eq!(r["href"], "https://example.com/a/b?x=1&q=naruto");
    assert_eq!(r["items"][1]["id"], "9");
    assert_eq!(r["cached"]["v"], 1);
    assert_eq!(r["b64"], "aMOpbGxv");
    assert_eq!(r["atob"], "xyz");
    assert_eq!(r["match"], "Shingeki no Kyojin");
    assert_eq!(r["evald"], 3);
    assert_eq!(r["uni"], "Ünïcode café");
    let err = p.call("getStreamUrls", json!(["1", "1"]), None, Duration::from_secs(5)).await.unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    let err = p.call("hang", json!([]), None, Duration::from_millis(300)).await.unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    // Still usable afterwards.
    assert!(p.call("getEpisodes", json!(["x"]), None, Duration::from_secs(5)).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn live_providers_search() {
    if std::env::var("TSUKIMI_LIVE").ok().as_deref() != Some("1") {
        return;
    }
    init();
    let dir = std::env::temp_dir().join("tsukimi-live-reg");
    let store =
        tsukimi_core::registry::RegistryStore::new(dir, tsukimi_core::registry::DEFAULT_REGISTRY.into(), Http::new());
    let out = store.load("4.0.0", |p| p.kind() == "anime" && !p.mature).await.unwrap();
    let only = std::env::var("TSUKIMI_ONLY").ok();
    let mut handles = vec![];
    for m in out.modules {
        if only.as_deref().is_some_and(|o| o != m.entry.id) {
            continue;
        }
        handles.push(tokio::spawn(async move {
            let started = std::time::Instant::now();
            let p = match js::JsProvider::load(&m.entry.id, m.source).await {
                Ok(p) => p,
                Err(e) => return format!("{:<10} LOAD FAIL {e:#}", m.entry.id),
            };
            let r = p.call("search", json!([{ "query": "Frieren" }]), None, Duration::from_secs(30)).await;
            let id = p
                .call(
                    "resolveShowId",
                    json!(["Frieren: Beyond Journey's End", "Sousou no Frieren", "sub"]),
                    None,
                    Duration::from_secs(40),
                )
                .await;
            let eps = match &id {
                Ok(v) if v.is_string() => p
                    .call("getEpisodes", json!([v, "sub"]), None, Duration::from_secs(30))
                    .await
                    .map(|e| e["episodes"].as_array().map(|a| a.len()).unwrap_or(0))
                    .map_err(|e| e.to_string()),
                _ => Err("no id".into()),
            };
            format!(
                "{:<10} search={:?} id={:?} eps={:?} {}ms",
                m.entry.id,
                r.map(|v| v.as_array().map(|a| a.len()).unwrap_or(0)).map_err(|e| e.to_string()),
                id.map_err(|e| e.to_string()),
                eps,
                started.elapsed().as_millis()
            )
        }));
    }
    for h in handles {
        println!("{}", h.await.unwrap());
    }
}
