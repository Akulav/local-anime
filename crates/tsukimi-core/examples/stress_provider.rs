//! Calls one live provider many times from parallel callers, to catch runtime-level crashes.
//!
//!   METHOD=resolveShowId cargo run --release -p tsukimi-core --example stress_provider -- animeya 3 25
//!
//! Args: provider id, parallel callers, rounds; METHOD = search (default) or resolveShowId. Before
//! `JsProvider::call` was serialized, `animeya 2` with resolveShowId died with STATUS_HEAP_CORRUPTION.
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tsukimi_core::{anilist::Anilist, http::Http, js, registry, store::Store};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tsukimi_core::init_logging();
    let mut args = std::env::args().skip(1);
    let id = args.next().unwrap_or_else(|| "mkissa".into());
    let par: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(40);
    let http = Http::new();
    let dir = std::env::temp_dir().join("tsukimi-stress-provider");
    std::fs::create_dir_all(&dir)?;
    let store = Arc::new(Store::open(&dir.join("t.db"))?);
    js::init_host(http.clone(), Anilist::new(http.clone(), store));
    let reg = registry::RegistryStore::new(dir.join("reg"), registry::DEFAULT_REGISTRY.into(), http);
    let out = reg.load(tsukimi_core::VERSION, |p| p.kind() == "anime").await?;
    let m = out.modules.into_iter().find(|m| m.entry.id == id).expect("no such provider");
    let p = Arc::new(js::JsProvider::load(&id, m.source).await?);
    let titles =
        ["Jiu Se Lu", "From I's: Mou Hitotsu no Natsu no Monogatari", "Star Wars/Force no Kakusei", "Sword Art Online"];
    let mut hs = vec![];
    for t in 0..par {
        let p = p.clone();
        hs.push(tokio::spawn(async move {
            for i in 0..rounds {
                let q = titles[(i + t) % titles.len()];
                let method = std::env::var("METHOD").unwrap_or_else(|_| "search".into());
                let args = if method == "resolveShowId" { json!([q, q, "sub"]) } else { json!([{ "query": q }]) };
                let r = p.call(&method, args, None, Duration::from_secs(30)).await;
                println!("caller {t} round {i}: {}", r.map(|_| "ok".to_string()).unwrap_or_else(|e| e.to_string()));
            }
        }));
    }
    for h in hs {
        h.await?;
    }
    println!("survived");
    std::process::exit(0)
}
