//! Tauri runs sync commands on the UI thread, outside the tokio runtime. Methods reachable from
//! them must not call `tokio::spawn` directly (it panics there, and release builds abort).

use tsukimi_core::{Paths, Tsukimi};

#[test]
fn background_work_can_start_outside_the_runtime() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = std::env::temp_dir().join(format!("tsukimi-threads-{}", rand_suffix()));
    let app = rt.block_on(Tsukimi::start(Paths::under(dir.clone()))).unwrap();

    let a = app.clone();
    std::thread::spawn(move || {
        assert!(tokio::runtime::Handle::try_current().is_err(), "test must run outside the runtime");
        a.prefetch_next(1, "none".into(), "2".into());
        a.track_playback(
            1,
            "none".into(),
            "1".into(),
            "http://127.0.0.1:1/s/x/hls/bad".into(),
            Vec::new(),
            "Show".into(),
            None,
        );
        a.download(1, "none", &["1".to_string()], "Show", None, true).unwrap();
        a.stop_downloads();
        assert!(a.downloads().is_ok());
    })
    .join()
    .expect("calling from a non-runtime thread panicked");

    drop(rt);
    let _ = std::fs::remove_dir_all(&dir);
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
}
