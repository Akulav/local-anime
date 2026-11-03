//! End-to-end smoke run against the live internet:
//! search -> probe all providers -> resolve streams -> play through the local proxy.
//!
//!   cargo run -p dango-core --example e2e -- "frieren"

use std::time::{Duration, Instant};

use dango_core::{Dango, Paths};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dango_core::init_logging();
    let query = std::env::args().nth(1).unwrap_or_else(|| "frieren".into());
    let dir = std::env::temp_dir().join("dango-e2e");
    let app = Dango::start(Paths { cache: dir.join("cache"), data: dir }).await?;

    let t = Instant::now();
    let res = app.search(&query, 1).await?;
    let first = &res["media"][0];
    let id = first["id"].as_i64().expect("no results");
    println!(
        "search: {} results in {:?}; using #{id} {}",
        res["media"].as_array().map(|a| a.len()).unwrap_or(0),
        t.elapsed(),
        first["title"]["romaji"]
    );

    let t = Instant::now();
    let mut rx = app.subscribe_probes();
    let mut snap = app.probe(id).await?;
    while !(snap.done
        && snap.sources.iter().all(|s| !matches!(s.state, dango_core::resolver::SourceState::Verifying))
        && snap
            .sources
            .iter()
            .filter(|s| s.state == dango_core::resolver::SourceState::Ok)
            .all(|s| s.playable.is_some()))
    {
        match tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
            Ok(Ok(s)) if s.anilist_id == id => snap = s,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    println!("probe finished in {:?}", t.elapsed());
    for s in &snap.sources {
        println!(
            "  {:<10} {:<9} eps={:<4} playable={:<5} {:>6}ms score={:.0} {}",
            s.provider,
            format!("{:?}", s.state),
            s.episode_count,
            format!("{:?}", s.playable),
            s.latency_ms.unwrap_or(0),
            s.score,
            s.error.clone().unwrap_or_default()
        );
    }
    // Same fallback the UI uses: walk the ranking until a provider yields streams.
    let mut picked = None;
    for s in
        snap.sources.iter().filter(|s| s.state == dango_core::resolver::SourceState::Ok && s.playable != Some(false))
    {
        let eps = app.episodes(id, &s.provider).await?;
        let ep = eps.first().expect("no episodes").number.clone();
        let t = Instant::now();
        match app.streams(id, &s.provider, &ep).await {
            Ok(v) if v.iter().any(|x| x.kind != "iframe") => {
                println!("streams from {} ep {ep} in {:?}:", s.provider, t.elapsed());
                picked = Some(v);
                break;
            }
            Ok(_) => println!("  {} only has embeds, trying next", s.provider),
            Err(e) => println!("  {} failed ({e:#}), trying next", s.provider),
        }
    }
    let sources = picked.expect("no provider produced streams");
    for s in &sources {
        println!("  {} [{}] {} qualities, {} subs", s.name, s.kind, s.qualities.len(), s.subtitles.len());
    }
    let src = sources.iter().find(|s| s.kind != "iframe").expect("only iframe sources");
    let url = &src.qualities[0].url;
    let client = reqwest::Client::new();
    if src.kind == "hls" {
        let mut playlist = client.get(url).send().await?.text().await?;
        // Descend into the first variant if this is a master playlist.
        if playlist.contains("#EXT-X-STREAM-INF") {
            let child = playlist.lines().find(|l| l.starts_with("http")).unwrap().to_string();
            playlist = client.get(&child).send().await?.text().await?;
        }
        let seg = playlist.lines().find(|l| l.starts_with("http")).expect("no segments").to_string();
        let t = Instant::now();
        let bytes = client.get(&seg).send().await?.bytes().await?;
        println!("first segment: {} bytes in {:?}", bytes.len(), t.elapsed());
        let t = Instant::now();
        let again = client.get(&seg).send().await?.bytes().await?;
        println!("cached segment: {} bytes in {:?}", again.len(), t.elapsed());
        tokio::time::sleep(Duration::from_secs(8)).await;
        for d in app.downloads() {
            println!("background download: {}/{} segments, {} KB", d.done, d.total, d.bytes / 1024);
        }
    } else {
        let r = client.get(url).header("range", "bytes=0-65535").send().await?;
        println!("mp4: HTTP {} {:?}", r.status(), r.headers().get("content-range"));
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    println!("cache: {:?}", app.cache_stats().await);
    Ok(())
}
