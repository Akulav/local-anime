//! Finished saved downloads live in the downloads folder, out of the cache's reach.

use tsukimi_core::stream::{sha_key, SavedSource, Upstream};
use tsukimi_core::{Paths, Tsukimi};

#[test]
fn kept_downloads_survive_cache_cleanup() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = std::env::temp_dir().join(format!("tsukimi-keep-{}", std::process::id()));
    let app = rt.block_on(Tsukimi::start(Paths::under(dir.clone()))).unwrap();
    let src = SavedSource {
        kind: "file".into(),
        spec: Upstream::new("https://example.com/show/ep1.mp4"),
        body: String::new(),
        final_url: String::new(),
        subtitles: vec![],
    };
    let name = format!("{}.bin", sha_key(&src.spec.url));
    let cached = dir.join("cache").join("files").join(&name);
    let kept = dir.join("downloads").join("files").join(&name);
    std::fs::write(&cached, b"video").unwrap();

    // a finished saved download moves to the downloads folder and still counts as on disk
    assert_eq!(app.stream.keep_files(&src, true), 1);
    assert!(kept.exists() && !cached.exists());
    assert!(app.stream.fully_kept(&src));
    assert_eq!(app.stream.disk_progress(&src).done, 5);

    // "Clear everything" and the janitor only see the cache
    let r = rt.block_on(app.clear_cache(true));
    assert_eq!(r.removed, 0, "{r:?}");
    rt.block_on(app.sweep_cache());
    assert!(kept.exists());
    assert_eq!(rt.block_on(app.cache_stats()).download_bytes, 5);

    // un-keeping puts it back in the cache, where it expires like any cached video
    assert_eq!(app.stream.keep_files(&src, false), 1);
    assert!(cached.exists() && !kept.exists());
    assert_eq!(app.stream.keep_files(&src, false), 0, "nothing left to move");

    drop(app);
    // Never drop the runtime: background provider loading may still be running QuickJS, and
    // cancelling that mid-flight corrupts the heap (see threads.rs).
    std::mem::forget(rt);
    let _ = std::fs::remove_dir_all(&dir);
}
