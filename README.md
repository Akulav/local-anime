<p align="center">
  <img src="app/icons/icon.png" width="112" alt="dango" />
</p>

<h1 align="center">dango</h1>

<p align="center">
  A fast, native desktop anime player for Windows and Linux.<br/>
  It finds a working source, starts playback right away, and buffers the whole episode to disk.
</p>

<p align="center">
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/License-MIT-ff8fb8?style=for-the-badge" alt="MIT"/></a>
  <img src="https://img.shields.io/badge/Rust-Tauri%202-ff8fb8?style=for-the-badge" alt="Rust + Tauri"/>
  <img src="https://img.shields.io/badge/Windows%20%7C%20Linux-desktop-ff8fb8?style=for-the-badge" alt="platforms"/>
</p>

---

## What it does

- **Automatic source picking.** Searching already contacts every provider in the background for the top results. Opening a show runs all providers in parallel: each one finds the show, lists its episodes and **test-plays a stream**. Working sources are ranked by verified playback, episode coverage and latency, and the best one is used. You can switch to any other provider, stream or quality at any time.
- **Automatic failover.** If a quality, stream or provider fails during playback, dango moves on to the next one and keeps your position.
- **Real-time download to disk.** Playback goes through a local streaming cache. HLS episodes download ahead of the playhead in parallel, so seeking within the episode is instant and stalls are rare. MP4 sources download progressively and play from the partial file while the download continues.
- **Automatic cleanup.** The cache has a size limit and a maximum age. The oldest files are evicted (LRU) on startup and every 10 minutes, and partial downloads are cleared on launch.
- **Caching everywhere.** AniList responses (memory + SQLite), provider show IDs and episode lists, playlists, subtitles and cover images (on disk) are all cached, so the app is fast after the first load.
- **A native desktop app.** One small executable with a native window, no browser and no server to run. Built with Rust and Tauri, with a plain-JS UI and no framework.
- Continue watching, library, resume position, prepared next episode, autoplay, skip-intro (aniskip), subtitles (VTT/SRT/ASS → VTT), SUB/DUB, keyboard-driven player.

## Download

Grab the latest build from [Releases](https://github.com/serifpersia/dango/releases):

| Platform | File |
| --- | --- |
| Windows installer | `dango-vX.Y.Z-windows-x64-setup.exe` |
| Windows portable | `dango-vX.Y.Z-windows-x64-portable.exe` |
| Linux AppImage | `dango-vX.Y.Z-linux-x86_64.AppImage` |
| Debian / Ubuntu | `dango-vX.Y.Z-linux-amd64.deb` |
| Linux binary | `dango-vX.Y.Z-linux-x86_64.tar.gz` |

- **Windows** needs the WebView2 runtime, which ships with Windows 10/11 (the installer adds it if it's missing).
- **Linux** needs WebKitGTK 4.1 and GTK 3 (`libwebkit2gtk-4.1-0`, `libgtk-3-0`). The AppImage and .deb take care of this on most distros.
- **Portable mode:** put an empty file named `portable` next to the executable. Data and cache then live in a `data/` folder beside it.

## Keyboard

| Key | Action |
| --- | --- |
| `/` or `Ctrl+K` | Focus search |
| `Space` / `K` | Play / pause |
| `←` `→` (`Shift` = 30 s) | Seek 5 s |
| `J` / `L` | Seek 10 s |
| `↑` `↓` | Volume |
| `0`–`9` | Jump to 0–90 % |
| `F` / double-click | Fullscreen |
| `N` / `P` | Next / previous episode |
| `E` | Episode list |
| `C` | Subtitles menu |
| `S` | Skip intro / outro |
| `M` | Mute |
| `Esc` | Close menu → exit fullscreen → close player |

## Where content comes from

dango doesn't bundle any scrapers. Metadata comes from **AniList** (search, details, episode counts, artwork), with **aniskip** for intro timestamps. Streams come from the community **provider registry** ([serifpersia/dango-providers](https://github.com/serifpersia/dango-providers)): small JavaScript modules that dango downloads, checks against their SHA-256 hash, caches, and runs in an embedded JavaScript engine. Providers update independently of the app. You can point dango at another registry in *Settings → Providers*.

Provider modules are code from that repository and run with the app's permissions. Only use registries you trust as much as the app itself.

## Building from source

Requirements: a recent stable Rust toolchain. On Windows, the MSVC build tools. On Linux:

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev
```

Run the app:

```bash
cargo run -p dango
```

Optimized single executable (`target/release/dango[.exe]`):

```bash
cargo build --release -p dango
```

Installers (NSIS / AppImage / deb) via the Tauri CLI:

```bash
cd app && npx @tauri-apps/cli@2 build
```

Tests, plus the opt-in live provider check:

```bash
cargo test --workspace
```

```bash
cargo run -p dango-core --example e2e -- "frieren"
```

`e2e` runs the whole pipeline against the live internet: search, provider probing, stream resolution, playing through the local proxy and the background download.

## Releases

Every push to `main` runs [`.github/workflows/release.yml`](.github/workflows/release.yml), which:

1. runs format, clippy and tests on Windows and Linux,
2. computes the next version (patch by default; put `#minor` or `#major` in the commit message for bigger bumps, or `[skip release]` to skip),
3. builds the Windows (installer + portable exe) and Linux (AppImage, deb, tarball) artifacts,
4. commits the version bump, tags `vX.Y.Z` and publishes a GitHub release with the files and a changelog.

## Project layout

```
crates/dango-core   engine: providers, resolver, stream cache, metadata, storage
app/                Tauri desktop shell + UI (app/ui, plain HTML/CSS/JS)
tools/js-vendor     regenerates the vendored cheerio/Buffer bundle and hls.js
```

See [CLAUDE.md](CLAUDE.md) for a deeper architecture tour.

## Credits & license

dango was created by [serifpersia](https://github.com/serifpersia/dango). This is a Rust rewrite of the original Node.js app and uses the same provider registry and module contract.

Released under the [MIT License](LICENSE).
