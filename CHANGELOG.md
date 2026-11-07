# Changelog

All notable changes to Tsukimi (formerly dango). The format follows [Keep a Changelog](https://keepachangelog.com/),
versions follow [Semantic Versioning](https://semver.org/). Note changes under **[Unreleased]** as you go;
`scripts\bump-version.ps1 [patch|minor|major|x.y.z]` turns that section into the new version; once that commit is
pushed to `main`, GitHub Actions tags, builds and publishes the release.

## [Unreleased]

## [4.1.1] - 2026-11-07

### Fixed
- **Crash while searching** (the window just vanished, nothing in the logs): two source checks running on the same
  provider at once corrupted its JavaScript engine's memory (STATUS_HEAP_CORRUPTION). Typing a search checks the top
  results in parallel, so some searches (e.g. "sword") crashed Tsukimi within seconds. Calls to one provider now run
  one after another, and providers load one at a time.
- A provider recursing very deeply could overflow the real thread stack and end the app silently; the engine's own
  stack limit now sits below the thread's, so it fails that one call with an error instead.
- CI on macOS: two tests stopped their runtime while provider modules were still loading in the background.

## [4.1.0] - 2026-11-07

### Changed
- **dango is now Tsukimi** ("moon-viewing"): new name, icon and colours. Installing Tsukimi over dango removes dango's
  program and shortcuts, and Tsukimi moves dango's library, watch progress, settings and downloads over on its first
  start. The credits stay: made by Akulav, inspired by dango by serifpersia; anime sources from the dango provider
  registry.
- **One file per system, the same file installs and updates.** Windows: `Tsukimi-setup.exe`, a proper setup wizard
  (Inno Setup: per user, no admin rights, Start menu and optional desktop shortcut, uninstaller in Settings → Apps that
  asks before deleting your library and downloads). Linux: `tsukimi-linux-x64`, a single program that installs itself
  with an app-menu entry on its first start. macOS (Apple Silicon): `Tsukimi-macos-arm64.dmg`, new. The portable exe,
  AppImage, .deb and tarball are gone.
- Downloads are permanent: a finished download moves out of the cache into its own downloads folder, which cache
  cleanup, the size limit and "Clear everything" never touch. Only deleting it (Downloads page) removes it. Deleting a
  downloaded episode or a whole show asks for a second click.
- The Downloads page separates what you downloaded from episodes cached while watching (collapsed, with their
  expiry), shows the downloads folder, and has a "delete all of this show" button.
- Releases are built for Windows, Linux and macOS by GitHub Actions; the release notes come from this changelog and
  every file is listed in `SHA256SUMS.txt`.
- Cleaner, quicker UI: moon-gold accent on a night-indigo background, a "by Akulav" mark in the sidebar, no blur on
  every poster badge, long episode lists skip off-screen rows, the Downloads page only redraws when something changed.

### Added
- **Updates.** Tsukimi checks GitHub for a new release 20 seconds after start and every 6 hours (Settings → Updates
  can turn that off). An **Update** button appears in the sidebar when there is one; Settings and About have **Check
  for updates** and **Update to x.y.z** with the release notes. The download is checked against the release's
  `SHA256SUMS.txt` (a mismatch installs nothing), then Windows runs the setup silently on the install folder and
  restarts Tsukimi; Linux and macOS swap the program in place and restart it.
- Downloads are more reliable: when the episode's provider fails, the download tries the other working providers in
  rank order; a failed download retries by itself (after 1, 3, 10, 30 minutes, 1 and 3 hours) and again at every start;
  a download that ends with parts missing counts as failed instead of looking finished; downloads always use the
  SUB/DUB mode they were started in.
- Starting Tsukimi again while it runs brings the open window to the front instead of a second copy.
- `tsukimi --version`; Linux: `--here` (run without installing) and `--uninstall [--delete-data]`.

### Fixed
- Settings: every visit added another set of listeners to the page, so a change was saved several times and the cache
  buttons ran once per earlier visit.
- Settings: "Update providers" threw an error when it failed instead of re-enabling the button.
- The downloads poll could overlap itself on a slow disk.
- A provider whose first stream couldn't be prepared made the whole download fail; the next stream is tried now.
