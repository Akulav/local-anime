# CLAUDE.md

Guidance for working on this repository.

## What this is

dango is a desktop anime player written in Rust (Tauri 2), with Windows and Linux targets. It replaced
an earlier Node.js/React web app (see git history before the Rust rewrite). Streams come from the
remote **provider registry** (`serifpersia/dango-providers`). Those provider modules are JavaScript
and run unmodified inside an embedded QuickJS engine.

## Layout

```
Cargo.toml                 workspace; [workspace.package].version is THE app version (CI bumps it)
crates/dango-core/         all logic, no UI dependency
  src/lib.rs               `Dango` facade: wires services, background tasks, image localisation
  src/js.rs                QuickJS runtime per provider + `__host` functions (http, sleep, cache, url, crypto, anilist)
  js/prelude.js            Node/WHATWG shims (fetch, URL, timers, TextEncoder, AbortSignal…) and the `ctx` object
  js/vendor.js             GENERATED bundle of cheerio + buffer (tools/js-vendor), do not edit by hand
  src/registry.rs          registry parsing, SHA-256 verified module download, disk cache + fallback
  src/providers.rs         loads anime providers into JsProviders, proxy rules, catalog
  src/resolver.rs          parallel source probing, ranking, stream verification, playable source mapping
  src/stream.rs            local axum server: HLS rewrite, segment disk cache, background downloader, mp4 progressive cache, subs, images
  src/janitor.rs           cache stats, age expiry + LRU eviction
  src/anilist.rs           GraphQL client (token bucket, request coalescing, memory+SQLite cache)
  src/store.rs             SQLite: kv cache (TTL), settings, library, progress
  src/settings.rs          user settings (JSON in the settings table)
  tests/js_runtime.rs      offline runtime test + DANGO_LIVE=1 live provider test
  examples/e2e.rs          live end-to-end pipeline smoke run
app/                       Tauri shell (binary name `dango`)
  src/main.rs              tauri commands → Dango methods; forwards probe events as "probe"
  tauri.conf.json          window, CSP, bundle config (version comes from Cargo.toml)
  capabilities/default.json
  ui/                      index.html, app.css, app.js (plain ES module), vendor/hls.min.js
  icons/                   generated icon set
tools/js-vendor/           npm project that builds js/vendor.js and copies hls.js
.github/workflows/         ci.yml (fmt, clippy, tests), release.yml (version → build → tag → release)
```

## Commands

```bash
cargo run -p dango                                   # run the desktop app (debug)
cargo build --release -p dango                       # single optimized executable
cargo test --workspace                               # unit + offline runtime tests
DANGO_LIVE=1 cargo test -p dango-core --test js_runtime live -- --nocapture   # every provider vs the live sites
cargo run -p dango-core --example e2e -- "frieren"   # search → probe → streams → proxy → disk cache
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings      # CI gates
cd tools/js-vendor && npm install && node build.mjs  # regenerate vendor.js + app/ui/vendor/hls.min.js
cd app && npx @tauri-apps/cli@2 build                # installers (nsis | appimage, deb)
```

`RUST_LOG=debug` enables verbose logs (provider console output appears at trace/debug).
`DANGO_DATA_DIR=<dir>` overrides the data/cache location. A `portable` file next to the exe uses `./data`.

## How playback works (the important flow)

1. **Search** (`Dango::search`) queries AniList. Results 1–3 are passed to `Resolver::prewarm`, which
   runs the probe without stream verification (bounded by a semaphore of 2).
2. **Probe** (`Resolver::probe`) runs one task per eligible provider, all in parallel:
   `isDirectId(anilistId)` → else `resolveShowId(title variants)` → else `__resolveBySearch` (search +
   title matching, in `prelude.js`) → `getEpisodes`. Show IDs are cached for 7 days and episode lists
   for 30 minutes in the SQLite kv. Each state change is broadcast as a `ProbeSnapshot`, which the
   app emits to the UI as the `probe` event.
3. **Verify** (`spawn_verify`): for each provider that is OK, call `getStreamUrls` for the first
   episode and fetch the playlist (HLS) or the first KB (mp4) through the same upstream logic the
   player uses. Sets `playable`.
4. **Rank** (`Resolver::score`): state, then verified playback, then episode coverage vs AniList,
   then tier/soft subs/latency, with a bonus for the preferred provider. `best` is the top usable
   source.
5. **Streams** (`Resolver::streams`): provider links (absolute URLs or legacy
   `/api/proxy?url=&referer=&variant=` strings) become `Upstream` specs. These are base64url JSON
   inside local URLs: `http://127.0.0.1:{port}/s/{token}/{hls|file|sub|img|seg}/{spec}`.
6. **Stream server** (`stream.rs`): playlists are rewritten so every child resource comes back
   through the server. A VOD media playlist registers an `HlsJob`: N workers download segments from
   the playhead forward (the player's segment requests carry `?j=&i=` to move the playhead). Segments
   are content-addressed files under `cache/segments/xx/<sha>`, and concurrent requests for the same
   segment share one download. MP4 uses `FileJob`: a sequential download to `.part` with a watch
   channel watermark. Ranges below the watermark stream from disk, far seeks pass through.
7. **Background downloads** (`Dango::queue_downloads`, called by the show page with the resume
   episode + `predownloadEpisodes`): resolves streams, picks the same source and quality as the
   player, and calls `StreamInner::prefetch`. For HLS masters this picks the highest-BANDWIDTH
   variant (the player pins `hls.currentLevel` to the top level, so both hit the same job/segments).
   `Dango::streams` caches results for 90 minutes, so the player gets the same signed URLs the
   prefetch used. Jobs are never cancelled when the player closes. A job the player requested
   within 20 s is "playing" and takes priority, and other jobs run one at a time in queue order
   (`should_yield`). At most 16 jobs are kept (finished ones dropped first). Downloaded files are
   removed by the janitor after `cacheMaxAgeHours` (48 h default) since last access.
8. **UI failover** (`app.js` `Player.failover`): next quality → next stream → next provider (in rank
   order), keeping the playback position.

Upstream headers: the spec's referer/origin/ua/cookie, then the registry proxy rule matched by host
(`referer`, `origin`, `originFromReferer`, `scrape`, `cfCookie`, `mirrors`). The `cf_clearance`
cookie comes from settings.

## Invariants / gotchas

- **Never drop a QuickJS runtime.** `JsProvider` holds its context and runtime in `ManuallyDrop` on
  purpose. Freeing a runtime while host futures or timers are queued corrupts the heap
  (STATUS_HEAP_CORRUPTION). Superseded providers are leaked on registry refresh (unchanged modules
  are reused, so this is rare).
- **Never cancel an `async_with!` future.** Dropping it mid-flight frees JS values without the
  runtime lock. Timeouts are enforced inside JS (`__dango.call` races a host timer), not with
  `tokio::time::timeout`. Keep it that way.
- Host async functions spawn their real work on tokio (`tokio::spawn`), so network I/O isn't tied to
  the JS lock.
- `prelude.js` must stay compatible with the provider contract the Node version offered
  (`RemoteCtx` in the old `server/src/providers/remote-loader.ts`): `ctx.cache`, `logger`,
  `fetchText`, `fetchJson`, `proxyUrl`, `titleMatch`, `resolveBestShowId`, `anilist`, `kitsu`,
  `tmdb`, `request`, `cookies`, `crypto`, `scraping.fetch` (got-scraping-like), `curl`, `cheerio`.
  Provider modules also use global `fetch`, `AbortSignal.timeout`, `Buffer`, `URL`,
  `URLSearchParams`, `atob/btoa`, `setTimeout`, `eval`, `process.env`.
- `ctx.proxyUrl()` returns the legacy `/api/proxy?...` string. `resolver::link_spec` translates it,
  so don't change the format.
- `/api/embed-proxy` links (animepahe iframe embeds) are not supported and are skipped.
- Only `kind: "anime"` registry entries are loaded. Manga/TV/ASMR providers are ignored.
- The stream server only accepts its random token and refuses loopback/private/`file:` upstreams.
- CSP in `tauri.conf.json` allows `http://127.0.0.1:*` for media/img/connect and `blob:` workers
  (hls.js). If you add a new resource origin, update it.
- The UI has no build step. Inline event handlers are blocked by the CSP, so use `addEventListener`.
  Image fade-in uses a capturing `load` listener.
- Version: only edit `[workspace.package].version` in the root `Cargo.toml`. The release workflow
  bumps it automatically and commits `chore: release vX.Y.Z [skip ci]`.

## Adding things

- **New Tauri command:** a method on `Dango` (core), a `#[tauri::command]` in `app/src/main.rs`,
  register it in `generate_handler!`, then call `invoke('name', {camelCaseArgs})` from `app.js`
  (Tauri maps camelCase args to snake_case params).
- **New setting:** a field in `Settings` (with a default), clamp it in `normalized()`, a control in
  `settingsView()` (`data-key` = camelCase field name).
- **New host capability for providers:** a function on `__host` in `js.rs::install_host`, exposed
  through `prelude.js`.
