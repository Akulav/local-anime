// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use dango_core::resolver::Subtitle;
use dango_core::settings::Settings;
use dango_core::store::Progress;
use dango_core::{Dango, Paths};
use serde_json::{json, Value};
use tauri::{Emitter, Manager, State};

type Res<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
fn boot(d: State<'_, Arc<Dango>>) -> Value {
    json!({
        "version": dango_core::VERSION,
        "settings": d.get_settings(),
        "paths": d.paths,
        "platform": std::env::consts::OS,
    })
}

#[tauri::command]
async fn home(d: State<'_, Arc<Dango>>) -> Res<Value> {
    d.home().await.map_err(err)
}

#[tauri::command]
async fn search(d: State<'_, Arc<Dango>>, query: String, page: Option<u32>) -> Res<Value> {
    d.search(&query, page.unwrap_or(1)).await.map_err(err)
}

#[tauri::command]
async fn details(d: State<'_, Arc<Dango>>, id: i64) -> Res<Value> {
    d.details(id).await.map_err(err)
}

#[tauri::command]
async fn probe(d: State<'_, Arc<Dango>>, id: i64) -> Res<Value> {
    d.probe(id).await.map_err(err).and_then(|s| serde_json::to_value(s).map_err(|e| e.to_string()))
}

#[tauri::command]
async fn episodes(d: State<'_, Arc<Dango>>, id: i64, provider: String) -> Res<Value> {
    let eps = d.episodes(id, &provider).await.map_err(err)?;
    Ok(serde_json::to_value(eps).unwrap_or_default())
}

#[tauri::command]
async fn streams(
    d: State<'_, Arc<Dango>>,
    id: i64,
    provider: String,
    episode: String,
    refresh: Option<bool>,
) -> Res<Value> {
    let d = d.inner().clone();
    let s = d.streams(id, &provider, &episode, refresh.unwrap_or(false)).await.map_err(err)?;
    Ok(serde_json::to_value(s).unwrap_or_default())
}

#[tauri::command(async)]
fn prefetch_next(d: State<'_, Arc<Dango>>, id: i64, provider: String, episode: String) {
    d.inner().prefetch_next(id, provider, episode);
}

#[tauri::command(async)]
fn download(
    d: State<'_, Arc<Dango>>,
    id: i64,
    provider: String,
    episodes: Vec<String>,
    title: String,
    cover: Option<String>,
    pinned: bool,
) -> Res<()> {
    d.inner().download(id, &provider, &episodes, &title, cover.as_deref(), pinned).map_err(err)
}

#[tauri::command(async)]
#[allow(clippy::too_many_arguments)]
fn track_playback(
    d: State<'_, Arc<Dango>>,
    id: i64,
    provider: String,
    episode: String,
    url: String,
    subtitles: Vec<Subtitle>,
    title: String,
    cover: Option<String>,
) {
    d.inner().track_playback(id, provider, episode, url, subtitles, title, cover);
}

#[tauri::command(async)]
fn local_source(d: State<'_, Arc<Dango>>, id: i64, episode: String) -> Value {
    serde_json::to_value(d.local_source(id, &episode)).unwrap_or_default()
}

#[tauri::command(async)]
fn downloads(d: State<'_, Arc<Dango>>) -> Res<Value> {
    d.downloads().map(|v| serde_json::to_value(v).unwrap_or_default()).map_err(err)
}

#[tauri::command]
async fn delete_download(d: State<'_, Arc<Dango>>, key: String) -> Res<()> {
    d.delete_download(&key).await.map_err(err)
}

#[tauri::command(async)]
fn pin_download(d: State<'_, Arc<Dango>>, key: String, pinned: bool) -> Res<()> {
    d.pin_download(&key, pinned).map_err(err)
}

#[tauri::command(async)]
fn resume_download(d: State<'_, Arc<Dango>>, key: String) {
    d.resume_download(&key);
}

#[tauri::command(async)]
fn stop_downloads(d: State<'_, Arc<Dango>>) {
    d.stop_downloads();
}

#[tauri::command]
async fn skip_times(d: State<'_, Arc<Dango>>, mal_id: i64, episode: String, duration: f64) -> Res<Value> {
    d.skip_times(mal_id, &episode, duration).await.map_err(err)
}

#[tauri::command]
fn library(d: State<'_, Arc<Dango>>) -> Res<Value> {
    d.library().map(|v| serde_json::to_value(v).unwrap_or_default()).map_err(err)
}

#[tauri::command]
fn library_add(d: State<'_, Arc<Dango>>, id: i64, title: String, cover: Option<String>) -> Res<()> {
    d.library_add(id, &title, cover.as_deref()).map_err(err)
}

#[tauri::command]
fn library_remove(d: State<'_, Arc<Dango>>, id: i64) -> Res<()> {
    d.library_remove(id).map_err(err)
}

#[tauri::command]
fn save_progress(d: State<'_, Arc<Dango>>, progress: Progress) -> Res<()> {
    d.save_progress(progress).map_err(err)
}

#[tauri::command]
fn continue_watching(d: State<'_, Arc<Dango>>) -> Res<Value> {
    d.continue_watching().map(|v| serde_json::to_value(v).unwrap_or_default()).map_err(err)
}

#[tauri::command]
fn remove_progress(d: State<'_, Arc<Dango>>, id: i64) -> Res<()> {
    d.remove_progress(id).map_err(err)
}

#[tauri::command]
fn get_settings(d: State<'_, Arc<Dango>>) -> Settings {
    d.get_settings()
}

#[tauri::command]
async fn set_settings(d: State<'_, Arc<Dango>>, settings: Settings) -> Res<Settings> {
    let d = d.inner().clone();
    d.set_settings(settings).await.map_err(err)
}

#[tauri::command]
fn providers(d: State<'_, Arc<Dango>>) -> Value {
    serde_json::to_value(d.provider_catalog()).unwrap_or_default()
}

#[tauri::command]
async fn refresh_providers(d: State<'_, Arc<Dango>>) -> Res<Value> {
    d.refresh_providers().await.map_err(err)?;
    Ok(serde_json::to_value(d.provider_catalog()).unwrap_or_default())
}

#[tauri::command]
async fn cache_stats(d: State<'_, Arc<Dango>>) -> Res<Value> {
    Ok(serde_json::to_value(d.cache_stats().await).unwrap_or_default())
}

#[tauri::command]
async fn clear_cache(d: State<'_, Arc<Dango>>, images: bool) -> Res<Value> {
    Ok(serde_json::to_value(d.clear_cache(images).await).unwrap_or_default())
}

#[tauri::command]
async fn sweep_cache(d: State<'_, Arc<Dango>>) -> Res<Value> {
    Ok(serde_json::to_value(d.sweep_cache().await).unwrap_or_default())
}

/// Release builds abort on panic and have no console, so a crash would leave no trace.
/// Record it in `<data>/crash.log` first.
fn install_crash_log() {
    let Ok(paths) = Paths::resolve() else { return };
    let log = paths.data.join("crash.log");
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let entry = format!("--- dango {} crashed (unix {secs}, thread {thread})\n{info}\n{bt}\n", dango_core::VERSION);
        let _ = std::fs::create_dir_all(&paths.data);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            use std::io::Write;
            let _ = f.write_all(entry.as_bytes());
        }
        default(info);
    }));
}

fn main() {
    dango_core::init_logging();
    install_crash_log();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let paths = Paths::resolve()?;
            let dango = tauri::async_runtime::block_on(Dango::start(paths))?;
            // Forward live source-probe updates to the UI.
            let mut rx = dango.subscribe_probes();
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(snap) => {
                            let _ = handle.emit("probe", snap);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
            app.manage(dango);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            boot,
            home,
            search,
            details,
            probe,
            episodes,
            streams,
            prefetch_next,
            download,
            track_playback,
            local_source,
            downloads,
            delete_download,
            pin_download,
            resume_download,
            stop_downloads,
            skip_times,
            library,
            library_add,
            library_remove,
            save_progress,
            continue_watching,
            remove_progress,
            get_settings,
            set_settings,
            providers,
            refresh_providers,
            cache_stats,
            clear_cache,
            sweep_cache,
        ])
        .run(tauri::generate_context!())
        .expect("error while running dango");
}
