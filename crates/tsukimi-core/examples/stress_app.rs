//! Drives the whole app like a busy user (searches with prewarm, probes with verification,
//! stream resolution) for a while, to catch process-level crashes that leave no log.
//!
//!   cargo run --release -p tsukimi-core --example stress_app -- 10
//!
//! Arg: minutes to run (default 5). Prints "survived" at the end; a fail-fast shows only as the
//! exit code (run under `cmd /c "… & echo %ERRORLEVEL%"`).
use std::time::{Duration, Instant};

use tsukimi_core::{Paths, Tsukimi};

const QUERIES: &[&str] = &[
    "frieren",
    "one piece",
    "naruto",
    "bleach",
    "gundam",
    "girls und panzer",
    "cyberpunk",
    "black clover",
    "spy x family",
    "jujutsu kaisen",
    "attack on titan",
    "demon slayer",
    "dandadan",
    "chainsaw man",
    "monogatari",
    "fate",
    "made in abyss",
    "mushishi",
    "vinland saga",
    "dr stone",
];

fn main() -> anyhow::Result<()> {
    // FLAVOR=current runs everything on one thread (isolates threading bugs).
    let rt = if std::env::var("FLAVOR").as_deref() == Ok("current") {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    };
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tsukimi_core::init_logging();
    let minutes: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let dir = std::env::temp_dir().join(format!("tsukimi-stress-app-{}", std::process::id()));
    let app = Tsukimi::start(Paths::under(&dir)).await?;
    app.wait_providers(Duration::from_secs(60)).await;
    let end = Instant::now() + Duration::from_secs(minutes * 60);
    let mut round = 0usize;
    while Instant::now() < end {
        let q = QUERIES[round % QUERIES.len()];
        round += 1;
        let res = match app.search(q, 1).await {
            Ok(r) => r,
            Err(e) => {
                println!("search {q}: {e:#}");
                continue;
            }
        };
        let ids: Vec<i64> =
            res["media"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_i64()).take(4).collect();
        // Open several shows at once, like clicking through results quickly.
        let probes = ids.iter().map(|&id| {
            let app = app.clone();
            async move {
                let _ = app.details(id).await;
                let snap = app.probe(id).await;
                tokio::time::sleep(Duration::from_secs(4)).await;
                if let Ok(snap) = snap {
                    for s in snap.sources.iter().take(3) {
                        if let Ok(eps) = app.episodes(id, &s.provider).await {
                            if let Some(ep) = eps.first() {
                                let _ = app.streams(id, &s.provider, &ep.number, false).await;
                            }
                        }
                    }
                }
            }
        });
        futures::future::join_all(probes).await;
        println!("round {round} ({q}): ok");
    }
    println!("survived {round} rounds");
    std::process::exit(0)
}
