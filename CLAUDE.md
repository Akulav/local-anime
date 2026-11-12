# CLAUDE.md

Guidance for working on this repository.

## What this is

Tsukimi (formerly dango) is a desktop anime player written in Rust (Tauri 2) for Windows, Linux and macOS (Apple
Silicon). It replaced an earlier Node.js/React web app (see git history before the Rust rewrite). Streams come from the
remote **provider registry** (`serifpersia/dango-providers`). Those provider modules are JavaScript and run unmodified
inside an embedded QuickJS engine.

Credits must stay intact: "Made by Akulav, inspired by dango by serifpersia" and "Anime sources available on the dango
provider registry" appear in the README, the About page (`aboutView` in `app.js`) and the settings footer; "Made by
Akulav" also in the sidebar (`#rail .by`), the setup and the macOS Info.plist. `LICENSE` keeps serifpersia's original
MIT notice. The rename to Tsukimi does not touch the registry contract: `minDango` in registry entries, `process.platform
= 'dango'` and the internal `__dango` object in `prelude.js` stay as they are.

## Layout

```
Cargo.toml                 workspace; [workspace.package].version is THE app version (scripts/bump-version.ps1)
CHANGELOG.md               release notes source: note changes under [Unreleased]
crates/tsukimi-core/       all logic, no UI dependency
  src/lib.rs               `Tsukimi` facade + `Paths` (data / cache / downloads): services, background tasks, image localisation
  src/migrate.rs           first start after the rename: moves dango's data + cache folders, dango.db -> tsukimi.db
  src/updater.rs           GitHub release check, SHA256SUMS-verified download, Windows setup / unix swap + relaunch
  src/js.rs                QuickJS runtime per provider + `__host` functions (http, sleep, cache, url, crypto, anilist)
  js/prelude.js            Node/WHATWG shims (fetch, URL, timers, TextEncoder, AbortSignal…) and the `ctx` object
  js/vendor.js             GENERATED bundle of cheerio + buffer (tools/js-vendor), do not edit by hand
  src/registry.rs          registry parsing, SHA-256 verified module download, disk cache + fallback
  src/providers.rs         loads anime providers into JsProviders, proxy rules, catalog
  src/resolver.rs          parallel source probing, ranking, stream verification, playable source mapping
  src/stream.rs            local axum server: HLS rewrite, segment disk cache, background downloader, mp4 progressive cache,
                           subs, images, and the downloads folder (`keep_files`)
  src/downloads.rs         persistent download queue, provider fallback, auto-retry, saved sources, playback from disk
  src/janitor.rs           cache stats, age expiry + LRU eviction (cache folder only)
  src/anilist.rs           GraphQL client (token bucket, request coalescing, memory+SQLite cache)
  src/store.rs             SQLite: kv cache (TTL), settings, library, progress, downloads
  src/settings.rs          user settings (JSON in the settings table)
  tests/js_runtime.rs      offline runtime test + TSUKIMI_LIVE=1 live provider test
  tests/downloads.rs       finished downloads leave the cache and survive cleanup
  tests/threads.rs         sync commands on a non-runtime thread must not panic
  examples/e2e.rs          live end-to-end pipeline smoke run
  examples/stress_app.rs   minutes of parallel search/probe/streams: catches heap corruption (prints "survived")
app/                       Tauri shell (package + binary `tsukimi`)
  src/main.rs              tauri commands → Tsukimi methods; probe events; single instance; --version
  src/install.rs           Linux/macOS self-install into a folder the user picks (zenity/kdialog/osascript), --here, --install-dir, --uninstall
  tauri.conf.json          window, CSP, icons (bundle inactive: releases use Inno Setup / plain binaries)
  installer/               tsukimi.iss (Windows setup = update), Info.plist (macOS app), wizard-*.bmp
  capabilities/default.json
  ui/                      index.html, app.css, app.js (plain ES module), vendor/hls.min.js
  icons/                   source.svg + the generated set (png, ico, icns)
scripts/                   bump-version.ps1, release.ps1, release-notes.ps1, installer-images.ps1
tools/js-vendor/           npm project that builds js/vendor.js and copies hls.js
.github/workflows/         ci.yml (fmt, clippy, tests on 3 systems, setup compiles), release.yml (version → tests → builds → tag → release)
```

## Commands

```bash
cargo run -p tsukimi                                   # run the desktop app (debug)
cd app && npx @tauri-apps/cli@2 build --no-bundle      # optimized program → target/release/tsukimi(.exe)
cargo test --workspace                                 # unit + offline tests
TSUKIMI_LIVE=1 cargo test -p tsukimi-core --test js_runtime live -- --nocapture   # every provider vs the live sites
cargo run -p tsukimi-core --example e2e -- "frieren"   # search → probe → streams → proxy → disk cache
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings          # CI gates
node --check app/ui/app.js                             # CI gate for the UI
iscc /DAppVersion=x.y.z /DExe=<abs path>\tsukimi.exe app\installer\tsukimi.iss  # Windows setup (Inno Setup 6)
cd tools/js-vendor && npm install && node build.mjs    # regenerate vendor.js + app/ui/vendor/hls.min.js
```

`RUST_LOG=debug` enables verbose logs (provider console output appears at trace/debug).
**All data lives in the install folder** (`Paths::resolve`: `<program folder>/data`, on macOS next to Tsukimi.app; owner's rule, like akulav-mc-launcher): library, settings, logs, cache, downloads and the webview's data (`data/webview`, set on the window in `main.rs`; macOS WKWebView can't be moved). `Paths::migrate_legacy` moves pre-4.2 data from the OS per-user folders (and dango's) into an installed copy's folder (marker `.tsukimi-install`, written by the setup / self-install). `TSUKIMI_DATA_DIR=<dir>` overrides.
On Windows git may not be on PATH: GitHub Desktop's copy is under `%LOCALAPPDATA%\GitHubDesktop\app-*\resources\app\git\cmd`.

## Releases and updates

- Version: only edit `[workspace.package].version` in the root `Cargo.toml` (use `scripts/bump-version.ps1`, which also
  fixes `Cargo.lock` and opens the CHANGELOG section). Pushing a new version to `main` publishes a release (tag `vX.Y.Z`
  on that commit). The release body is that version's CHANGELOG section (`scripts/release-notes.ps1` fails on an empty
  one). The workflow never pushes commits.
- One file per system, the same file installs and updates: `Tsukimi-setup.exe`, `tsukimi-linux-x64`,
  `tsukimi-macos-arm64` (exactly three programs) and `SHA256SUMS.txt`. Linux and macOS install themselves on first start
  (`install.rs`: into a folder the user picks, default ~/.local/share/tsukimi or ~/Applications/Tsukimi/Tsukimi.app;
  remembered in `<config>/tsukimi/install-dir`). Renaming any of them
  breaks updates of installed copies (`updater::asset_name`).
- The setup's `AppId` GUID in `tsukimi.iss` must never change. The setup removes an old dango NSIS install (program,
  shortcuts, uninstall key) but never its data: the app migrates that (`migrate.rs`).
- A copy whose exe path contains a `target` folder is "self built" and never replaces itself.

## How playback works (the important flow)

1. **Search** (`Tsukimi::search`) queries AniList. Results 1–3 are passed to `Resolver::prewarm`, which runs the probe
   without stream verification (bounded by a semaphore of 2).
2. **Probe** (`Resolver::probe`) runs one task per eligible provider, all in parallel: `isDirectId(anilistId)` → else
   `resolveShowId(title variants)` → else `__resolveBySearch` (search + title matching, in `prelude.js`) → `getEpisodes`.
   Show IDs are cached for 7 days and episode lists for 30 minutes in the SQLite kv. Each state change is broadcast as a
   `ProbeSnapshot`, which the app emits to the UI as the `probe` event.
3. **Verify** (`spawn_verify`): for each provider that is OK, call `getStreamUrls` for the first episode and fetch the
   playlist (HLS) or the first KB (mp4) through the same upstream logic the player uses. Sets `playable`.
4. **Rank** (`Resolver::score`): state, then verified playback, then episode coverage vs AniList, then tier/soft
   subs/latency, with a bonus for the preferred provider. `best` is the top usable source.
5. **Streams** (`Resolver::streams`, with an explicit SUB/DUB mode): provider links (absolute URLs or legacy
   `/api/proxy?url=&referer=&variant=` strings) become `Upstream` specs. These are base64url JSON inside local URLs:
   `http://127.0.0.1:{port}/s/{token}/{hls|file|sub|img|seg}/{spec}`.
6. **Stream server** (`stream.rs`): playlists are rewritten so every child resource comes back through the server. A
   VOD media playlist registers an `HlsJob`: N workers download segments from the playhead forward (the player's segment
   requests carry `?j=&i=` to move the playhead). Segments are content-addressed files under `cache/segments/xx/<sha>`
   (or `downloads/segments/xx/<sha>` once kept: lookups check the downloads folder first), and concurrent requests for
   the same segment share one download. MP4 uses `FileJob`: a sequential download to `.part` with a watch channel
   watermark. Ranges below the watermark stream from disk, far seeks pass through.
7. **Downloads** (`downloads.rs`). Every downloaded or queued episode is a row in the `downloads` table, keyed by
   `sha(anilistId:episode:mode)`. Its `source` column is a `SavedSource`: the media playlist as fetched (body + final
   URL + upstream spec with headers) and the subtitles as VTT. This is what makes downloads survive restarts and expired
   provider links: the saved playlist is re-rewritten locally (`/dl/{key}.m3u8`, subtitles `/dls/{key}.{i}.vtt`), so
   segment URLs, and therefore the content-addressed segment files, stay the same.
   - Rows come from three places: the Download button / `D` / *Download all* (`pinned`, kept until deleted), the show
     page pre-download (`predownloadEpisodes` from the resume point, not pinned), and `track_playback` (the player
     records what it streams, not pinned).
   - A single queue worker (`spawn_downloader`) handles one row at a time: resolve streams in the row's own mode (its
     provider first, then — same mode as the UI only — every other working provider from the probe, in rank order),
     `StreamInner::prepare` (best-BANDWIDTH variant, which the player pins too), then watch progress. No progress for
     3 min while nothing plays → retry once with refreshed links. `finished` re-counts the disk: parts missing = error.
     Pinned rows go ahead of pre-downloads.
   - A failed pinned row is retried automatically (`RETRY_AFTER`: 1, 3, 10, 30 min, 1 h, 3 h; in-memory, the 20 s
     ticker) and again at every start (`resume_downloads(true)`). "Pause all" bumps `Queue::generation`, which is how a
     stopped job tells a pause from a failure.
   - **Permanence:** when a pinned row completes, `keep_files(src, true)` moves its files from `cache/` to
     `downloads/` (rename, or copy via a temp name across drives). Un-pinning moves them back. At start, complete pinned
     rows still in the cache are moved (dango migration). The janitor and `clear_cache` only see `cache/`;
     `protected_files` additionally guards pinned rows' cache files while they download. `delete_download` removes both
     locations but skips files another pinned row shares.
   - A new `HlsJob` first stats every segment (downloads folder, then cache), so already-downloaded episodes show as
     complete immediately. A job the player requested within 20 s is "playing" and others yield to it (`should_yield`).
     Jobs are never cancelled when the player closes.
   - The player asks `local_source` first and plays the saved copy when anything of it is on disk.
   - Cleanup (`sweep_cache`): unpinned rows older than `cacheMaxAgeHours` (48 h default) since last use are deleted
     with their files, then the janitor applies age/size limits to the rest of the cache.
8. **UI failover** (`app.js` `Player.failover`): next quality → next stream → the same provider with refreshed links →
   next provider (in rank order), keeping the playback position.
9. **Updates** (`updater.rs`): `Tsukimi::start` cleans up the last update and checks 20 s after start, then every 6 h
   (setting `checkUpdates`). The UI reads `update_status` once a minute (and every 0.7 s while installing);
   `install_update` downloads, verifies against `SHA256SUMS.txt`, then Windows starts the setup detached with
   `/SILENT /LAUNCH=1 /DIR=<exe dir>` and the app exits; Linux/macOS swap the binary (`.old` kept until next start) and
   relaunch through `sh -c "sleep 2; …"` (the single-instance plugin would otherwise hand the start back to us).

Upstream headers: the spec's referer/origin/ua/cookie, then the registry proxy rule matched by host (`referer`,
`origin`, `originFromReferer`, `scrape`, `cfCookie`, `mirrors`). The `cf_clearance` cookie comes from settings.

## Invariants / gotchas

- **Never drop a QuickJS runtime.** `JsProvider` holds its context and runtime in `ManuallyDrop` on purpose. Freeing a
  runtime while host futures or timers are queued corrupts the heap (STATUS_HEAP_CORRUPTION). Superseded providers are
  leaked on registry refresh (unchanged modules are reused, so this is rare).
- **Never cancel an `async_with!` future.** Dropping it mid-flight frees JS values without the runtime lock. Timeouts
  are enforced inside JS (`__dango.call` races a host timer), not with `tokio::time::timeout`. Keep it that way.
- **One call at a time per provider, one load at a time overall.** Two `async_with!` calls interleaving on the same
  context, or runtimes created/evaluated on several threads at once, corrupt the QuickJS heap (rquickjs 0.9,
  `parallel`). `JsProvider::call` holds `calls`, `JsProvider::load` holds the static `LOADING`. Regression check:
  `METHOD=resolveShowId cargo run --release -p tsukimi-core --example stress_provider -- animeya 3 25`.
- **All QuickJS work runs on one thread** (`js::engine()`, "tsukimi-js", a current-thread runtime): loads, calls
  (`on_engine`) and every runtime's `drive()`. Letting runtimes hop between tokio workers corrupted the heap within
  minutes of busy searching (0xC0000374). Host functions do network work on the app runtime (`HostServices::io`),
  never on the engine thread. Regression check: `cargo run --release -p tsukimi-core --example stress_app -- 8`
  must print "survived" (check the real exit code with `Start-Process -PassThru`; cmd expands `%ERRORLEVEL%` too
  early).
- Windows: `tsukimi.exe --watch-exit <pid>` is the hidden watcher started at launch; it writes abnormal exit codes
  to `crash.log` / `tsukimi.log`. WER may be disabled on users' machines, so it's the only trace of a fail-fast.
- QuickJS's stack limit (`set_max_stack_size`, 1 MiB) must stay below the thread stack (tokio workers: 2 MiB), or deep
  provider recursion overflows the real stack and the process dies without a trace.
- Tests that `Tsukimi::start` must not drop their tokio runtime (`std::mem::forget(rt)`): background provider loading
  would be cancelled mid-flight.
- A crash that leaves neither `crash.log` nor a log line is a fail-fast (heap corruption, stack overflow); get the exit
  code by running a release example under `cmd /c "… & echo %ERRORLEVEL%"` (0xC0000374 = heap corruption).
- Host async functions spawn their real work on the app runtime (`host().io.spawn`), so network I/O isn't tied to
  the JS lock or the engine thread.
- **Don't `tokio::spawn` from `Tsukimi` methods; use `self.rt.spawn`.** Plain `#[tauri::command]`s run on the UI thread
  outside the runtime, where `tokio::spawn` panics, and release builds use `panic = "abort"`, so the app just
  disappears (this crashed playback once, see `tests/threads.rs`). Commands that do real work are
  `#[tauri::command(async)]` or `async fn`. Panics are written to `<data>/crash.log` by the hook in `app/src/main.rs`.
- **Finished saved downloads must never live only in the cache folder.** Anything that writes media for a pinned row
  goes through the cache and is moved by `keep_files`; don't make the janitor aware of `downloads/` instead.
- `prelude.js` must stay compatible with the provider contract the Node version offered (`RemoteCtx` in the old
  `server/src/providers/remote-loader.ts`): `ctx.cache`, `logger`, `fetchText`, `fetchJson`, `proxyUrl`, `titleMatch`,
  `resolveBestShowId`, `anilist`, `kitsu`, `tmdb`, `request`, `cookies`, `crypto`, `scraping.fetch` (got-scraping-like),
  `curl`, `cheerio`. Provider modules also use global `fetch`, `AbortSignal.timeout`, `Buffer`, `URL`,
  `URLSearchParams`, `atob/btoa`, `setTimeout`, `eval`, `process.env`.
- `ctx.proxyUrl()` returns the legacy `/api/proxy?...` string. `resolver::link_spec` translates it, so don't change the
  format.
- `/api/embed-proxy` links (animepahe iframe embeds) are not supported and are skipped.
- Only `kind: "anime"` registry entries are loaded. Manga/TV/ASMR providers are ignored.
- The registry's `minDango` is compared with the app version, so versions keep counting from dango's (4.x), never
  restart at 1.0.
- The stream server only accepts its random token and refuses loopback/private/`file:` upstreams.
- CSP in `tauri.conf.json` allows `http://127.0.0.1:*` for media/img/connect and `blob:` workers (hls.js). If you add a
  new resource origin, update it. GitHub is reached from Rust only (updater), never from the webview.
- The UI has no build step. Inline event handlers are blocked by the CSP, so use `addEventListener`. Image fade-in uses
  a capturing `load` listener. Attach page listeners to the page's own root element (or remove them in `S.view.leave`):
  `#view` persists across pages, so listeners on it pile up.

## Adding things

- **New Tauri command:** a method on `Tsukimi` (core), a `#[tauri::command]` in `app/src/main.rs`, register it in
  `generate_handler!`, then call `invoke('name', {camelCaseArgs})` from `app.js` (Tauri maps camelCase args to
  snake_case params).
- **New setting:** a field in `Settings` (with a default), clamp it in `normalized()`, a control in `settingsView()`
  (`data-key` = camelCase field name).
- **New host capability for providers:** a function on `__host` in `js.rs::install_host`, exposed through `prelude.js`.
- **A change worth a release:** a line under `[Unreleased]` in `CHANGELOG.md`, then `scripts/bump-version.ps1`.
