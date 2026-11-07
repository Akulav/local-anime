//! Updates from the GitHub releases (Akulav/local-anime): checked 20 s after start and every
//! 6 hours (Settings → Updates can turn that off), and whenever the user presses Check for updates.
//!
//! A release has ONE file per system, and the same file installs and updates
//! (.github/workflows/release.yml). It is always checked against the release's SHA256SUMS.txt
//! first: nothing changes when the download or its checksum fails.
//!
//! - Windows: `Tsukimi-setup.exe` (Inno Setup, app/installer/tsukimi.iss). It is started on this
//!   install's folder (/SILENT: a progress window, no questions) and Tsukimi exits; the setup removes
//!   the old version (library, settings and downloads stay), installs the new one and starts it.
//! - Linux: `tsukimi-linux-x64`, macOS: `tsukimi-macos-arm64`: the program itself. It is downloaded,
//!   swapped in (this one becomes `.old`, removed at the next start) and started again; on macOS
//!   inside Tsukimi.app, which is re-signed (ad hoc) and reopened.
//!
//! A copy built on this computer (…/target/…) is never replaced.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::http::Http;
use crate::store::now_secs;

/// GitHub repository the releases come from.
pub const REPO: &str = "Akulav/local-anime";
/// Background check interval (the first check runs shortly after start-up).
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

fn user_agent() -> String {
    format!("Tsukimi/{} (+https://github.com/{REPO})", crate::VERSION)
}

/// The release file for this computer (Windows: the setup, which installs and updates alike).
pub fn asset_name() -> &'static str {
    if cfg!(windows) {
        "Tsukimi-setup.exe"
    } else if cfg!(target_os = "macos") {
        "tsukimi-macos-arm64"
    } else {
        "tsukimi-linux-x64"
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRelease {
    pub version: String,
    /// Release page on GitHub ("What's new").
    pub url: String,
    /// The release notes (markdown, shortened).
    pub notes: String,
    /// This computer's file (`asset_name`) and the checksum list.
    pub file: String,
    pub sums: String,
}

/// What the UI shows. `latest` is set only when a newer version exists.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current: String,
    pub latest: Option<AppRelease>,
    /// Unix seconds of the last finished check (0 = never).
    pub checked_at: i64,
    pub checking: bool,
    pub installing: bool,
    /// Download progress of the update, in bytes.
    pub downloaded: u64,
    pub size: u64,
    pub error: String,
    /// This copy was built on this computer: it can't update itself.
    pub self_built: bool,
}

pub struct Updater {
    http: Http,
    /// Where the update is downloaded (removed at the next start).
    work: PathBuf,
    state: RwLock<UpdateInfo>,
}

impl Updater {
    pub fn new(http: Http, work: PathBuf) -> Arc<Self> {
        let self_built = std::env::current_exe().map(|e| self_built(&e)).unwrap_or(false);
        Arc::new(Self {
            http,
            work,
            state: RwLock::new(UpdateInfo { current: crate::VERSION.into(), self_built, ..Default::default() }),
        })
    }

    pub fn info(&self) -> UpdateInfo {
        self.state.read().clone()
    }

    /// Asks GitHub for the latest release. Never fails: problems end up in `error`.
    pub async fn check(&self) -> UpdateInfo {
        {
            let mut s = self.state.write();
            if s.checking || s.installing {
                return s.clone();
            }
            s.checking = true;
        }
        let res = self.latest().await;
        let mut s = self.state.write();
        s.checking = false;
        s.checked_at = now_secs();
        match res {
            Ok(r) => {
                s.latest = r;
                s.error.clear();
            }
            Err(e) => s.error = format!("{e:#}"),
        }
        tracing::info!("update check: latest {:?} {}", s.latest.as_ref().map(|r| &r.version), s.error);
        s.clone()
    }

    async fn latest(&self) -> Result<Option<AppRelease>> {
        let resp = self
            .http
            .client()
            .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
            .header("User-Agent", user_agent())
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(20))
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None); // nothing published yet
        }
        let rel = parse_release(&resp.error_for_status()?.json().await?)?;
        // (a release without a file for this computer is not offered: nothing could be installed)
        Ok((newer(&rel.version, crate::VERSION) && !rel.file.is_empty() && !rel.sums.is_empty()).then_some(rel))
    }

    /// Downloads, verifies and starts the offered release. When this returns Ok the app must quit
    /// right away so the setup (Windows) or the new program (Linux, macOS) can take over.
    pub async fn install(&self) -> Result<()> {
        let rel = {
            let mut s = self.state.write();
            if s.installing {
                bail!("the update is already being installed");
            }
            let rel = s.latest.clone().ok_or_else(|| anyhow!("no update available, check again"))?;
            s.installing = true;
            s.downloaded = 0;
            s.size = 0;
            s.error.clear();
            rel
        };
        let res = self.install_inner(&rel).await;
        let mut s = self.state.write();
        if let Err(e) = &res {
            tracing::warn!("update failed: {e:#}");
            s.installing = false;
            s.error = format!("{e:#}");
        } else {
            tracing::info!("installing Tsukimi {}, quitting", rel.version);
        }
        res
    }

    async fn install_inner(&self, rel: &AppRelease) -> Result<()> {
        let exe = std::env::current_exe()?;
        if self_built(&exe) {
            bail!("this copy of Tsukimi was built on this computer: update it from the source instead");
        }
        tokio::fs::create_dir_all(&self.work).await?;
        let file = self.work.join(format!("{}-{}", rel.version, asset_name()));
        self.download(&rel.file, &file).await?;

        // integrity: the checksum list published with the release must match the download
        let sums = self
            .http
            .client()
            .get(&rel.sums)
            .header("User-Agent", user_agent())
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let expected = expected_hash(&sums, asset_name())
            .ok_or_else(|| anyhow!("{} is not listed in SHA256SUMS.txt", asset_name()))?;
        let f2 = file.clone();
        let actual = tokio::task::spawn_blocking(move || sha256_file(&f2)).await??;
        if actual != expected {
            let _ = std::fs::remove_file(&file);
            bail!("the download doesn't match the release's checksum, nothing was installed");
        }
        let app_dir = exe.parent().ok_or_else(|| anyhow!("cannot locate Tsukimi's folder"))?;
        #[cfg(windows)]
        {
            run_setup(&file, app_dir, &self.work.with_file_name("setup.log"))
        }
        #[cfg(not(windows))]
        {
            let _ = app_dir;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
            swap(&exe, &file)?;
            relaunch(&exe)
        }
    }

    async fn download(&self, url: &str, to: &Path) -> Result<()> {
        use futures::StreamExt;
        let resp = self.http.client().get(url).header("User-Agent", user_agent()).send().await?.error_for_status()?;
        self.state.write().size = resp.content_length().unwrap_or(0);
        let part = to.with_extension("part");
        let mut f = tokio::fs::File::create(&part).await?;
        let mut stream = resp.bytes_stream();
        let mut got = 0u64;
        loop {
            let chunk = tokio::time::timeout(Duration::from_secs(60), stream.next())
                .await
                .map_err(|_| anyhow!("the download stalled"))?;
            let Some(chunk) = chunk else { break };
            let chunk = chunk?;
            f.write_all(&chunk).await?;
            got += chunk.len() as u64;
            self.state.write().downloaded = got;
        }
        f.flush().await?;
        drop(f);
        tokio::fs::rename(&part, to).await?;
        Ok(())
    }

    /// (At start.) Removes what the last update left: the previous program (.old) and the
    /// downloaded setup / program.
    pub fn cleanup(&self) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::fs::remove_file(sibling(&exe, "old"));
        }
        let _ = std::fs::remove_dir_all(&self.work);
    }
}

/// "v2.10.1" -> [2, 10, 1] (a pre-release suffix is ignored for the numbers).
fn numbers(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|x| x.parse().unwrap_or(0))
        .collect()
}

/// Is `latest` newer than `current`?
pub fn newer(latest: &str, current: &str) -> bool {
    let (a, b) = (numbers(latest), numbers(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// The release as GitHub's API describes it ("latest release" JSON), with this computer's file.
fn parse_release(v: &Value) -> Result<AppRelease> {
    let tag = v["tag_name"].as_str().ok_or_else(|| anyhow!("GitHub's answer has no version"))?;
    let asset = |name: &str| -> String {
        v["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["browser_download_url"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let notes: String = v["body"].as_str().unwrap_or_default().chars().take(4000).collect();
    Ok(AppRelease {
        version: tag.trim_start_matches('v').to_string(),
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        notes,
        file: asset(asset_name()),
        sums: asset("SHA256SUMS.txt"),
    })
}

/// A program built on this computer (cargo / tauri: …/target/…) is never replaced by a download.
pub fn self_built(exe: &Path) -> bool {
    exe.components().any(|c| c.as_os_str() == "target")
}

/// Hash for `name` in a `sha256sum`-style list (`<hex>  <file>`, optional `*` binary marker).
fn expected_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (hash, n) = (it.next()?, it.next()?.trim_start_matches('*'));
        (n == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// The setup's options for an update (tsukimi.iss): on this folder, a progress window but no
/// questions, close a running Tsukimi, start it again when done (LAUNCH=1), with a log.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn setup_args(app_dir: &Path, log: &Path) -> Vec<String> {
    vec![
        "/SILENT".into(),
        "/SUPPRESSMSGBOXES".into(),
        "/NORESTART".into(),
        "/CLOSEAPPLICATIONS".into(),
        "/LAUNCH=1".into(),
        format!("/DIR={}", app_dir.display()),
        format!("/LOG={}", log.display()),
    ]
}

/// Starts the setup on its own (it outlives Tsukimi, which quits so its files can be replaced).
#[cfg(windows)]
fn run_setup(setup: &Path, app_dir: &Path, log: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let spawn =
        |flags: u32| std::process::Command::new(setup).args(setup_args(app_dir, log)).creation_flags(flags).spawn();
    // leave any job Tsukimi itself runs in, so the setup survives Tsukimi's exit; not every job allows it
    spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP))
        .map_err(|e| anyhow!("couldn't start the setup: {e}"))?;
    Ok(())
}

/// tsukimi -> tsukimi.old / tsukimi.exe -> tsukimi.exe.old
pub fn sibling(exe: &Path, ext: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(format!(".{ext}"));
    PathBuf::from(s)
}

/// Puts the new program in this one's place (this one is kept as .old until the next start removes
/// it). Linux and macOS: on Windows the setup replaces the program.
#[cfg_attr(windows, allow(dead_code))]
pub fn swap(exe: &Path, new: &Path) -> Result<()> {
    let old = sibling(exe, "old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| anyhow!("couldn't move this program aside: {e}"))?;
    if let Err(e) = std::fs::rename(new, exe).or_else(|_| std::fs::copy(new, exe).map(|_| ())) {
        let _ = std::fs::rename(&old, exe); // put it back: nothing changed
        bail!("couldn't put the new program in place: {e}");
    }
    Ok(())
}

/// The Tsukimi.app a macOS program runs from (…/Tsukimi.app/Contents/MacOS/tsukimi), if any.
#[cfg_attr(windows, allow(dead_code))]
pub fn app_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

/// Starts the new program once this one has ended (the single-instance guard would otherwise
/// hand the start back to us). macOS: re-sign the bundle (its seal covers the program) and reopen it.
#[cfg(not(windows))]
fn relaunch(exe: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("/bin/sh");
    match app_bundle(exe).filter(|_| cfg!(target_os = "macos")) {
        Some(bundle) => {
            let _ =
                std::process::Command::new("codesign").args(["--force", "--deep", "--sign", "-"]).arg(&bundle).status();
            cmd.arg("-c").arg("sleep 2; open -n \"$0\"").arg(&bundle);
        }
        None => {
            cmd.arg("-c").arg("sleep 2; exec \"$0\" --after-update").arg(exe);
        }
    }
    // its own process group: it must outlive Tsukimi
    cmd.process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().map_err(|e| anyhow!("couldn't start the new version: {e}. Start Tsukimi again to run it"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("4.10.0", "4.9.9"));
        assert!(newer("v4.1.1", "4.1.0"));
        assert!(!newer("4.1.0", "4.1.0"));
        assert!(!newer("4.0.9", "4.1.0"));
        assert!(newer("5", "4.99.99"));
        assert!(!newer("4.1.0-beta", "4.1.0"));
    }

    #[test]
    fn release_json_gives_this_computers_file() {
        let v = serde_json::json!({"tag_name": "v4.1.0", "html_url": "https://github.com/x/y/releases/tag/v4.1.0", "body": "notes", "assets": [
            {"name": "SHA256SUMS.txt", "browser_download_url": "https://dl/sums"},
            {"name": asset_name(), "browser_download_url": "https://dl/prog"},
            {"name": "other", "browser_download_url": "https://dl/other"}]});
        let r = parse_release(&v).unwrap();
        assert_eq!(
            (r.version.as_str(), r.file.as_str(), r.sums.as_str(), r.notes.as_str()),
            ("4.1.0", "https://dl/prog", "https://dl/sums", "notes")
        );
        assert!(parse_release(&serde_json::json!({})).is_err());
        // the old release layout (dango-vX-windows-x64-setup.exe, …) has nothing to install
        let old = parse_release(&serde_json::json!({"tag_name": "v4.0.4", "assets": [{"name": "dango-v4.0.4-windows-x64-setup.exe", "browser_download_url": "x"}]})).unwrap();
        assert!(old.file.is_empty());
    }

    #[test]
    fn finds_hash_in_release_sums() {
        let sums = "1fa1c6a24789a0a55ab9265706 4fe6fbd0b312c185f2c4b006df6f58bd7517f827  Tsukimi-setup.exe\r\n\
                    b774e08051dda5228007f80eb2dbb3ca22408f4b2f83450c468603b32f73819e  tsukimi-linux-x64\r\n\
                    B774E08051DDA5228007F80EB2DBB3CA22408F4B2F83450C468603B32F73819E *tsukimi-macos-arm64\n";
        assert_eq!(
            expected_hash(sums, "tsukimi-linux-x64").as_deref(),
            Some("b774e08051dda5228007f80eb2dbb3ca22408f4b2f83450c468603b32f73819e")
        );
        assert_eq!(
            expected_hash(sums, "tsukimi-macos-arm64").as_deref(),
            Some("b774e08051dda5228007f80eb2dbb3ca22408f4b2f83450c468603b32f73819e")
        );
        assert_eq!(expected_hash(sums, "Tsukimi-setup.exe"), None, "malformed hash is rejected");
        assert_eq!(expected_hash(sums, "other.zip"), None);
    }

    #[test]
    fn windows_updates_with_the_setup_on_this_folder() {
        if cfg!(windows) {
            assert_eq!(asset_name(), "Tsukimi-setup.exe");
        }
        let args = setup_args(Path::new("C:/Users/u/AppData/Local/Programs/Tsukimi"), Path::new("C:/t/setup.log"));
        assert!(args.contains(&"/SILENT".to_string()) && args.contains(&"/LAUNCH=1".to_string()));
        assert!(args.iter().any(|a| a == "/DIR=C:/Users/u/AppData/Local/Programs/Tsukimi"), "{args:?}");
        assert!(args.iter().any(|a| a == "/LOG=C:/t/setup.log"));
    }

    #[test]
    fn swap_puts_the_new_program_in_place_and_keeps_the_old() {
        let dir = std::env::temp_dir().join(format!("tsukimi-swap-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("tsukimi");
        std::fs::write(&exe, b"old").unwrap();
        let new = dir.join("download");
        std::fs::write(&new, b"new").unwrap();
        swap(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(sibling(&exe, "old")).unwrap(), b"old");
        // a missing download: nothing changes
        assert!(swap(&exe, &dir.join("missing")).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_built_here_are_never_replaced() {
        assert!(self_built(Path::new("D:/src/local-anime/target/release/tsukimi.exe")));
        assert!(!self_built(Path::new("C:/Users/u/AppData/Local/Programs/Tsukimi/tsukimi.exe")));
        assert_eq!(
            app_bundle(Path::new("/Applications/Tsukimi.app/Contents/MacOS/tsukimi")),
            Some(PathBuf::from("/Applications/Tsukimi.app"))
        );
        assert_eq!(app_bundle(Path::new("/home/u/.local/share/tsukimi/tsukimi")), None);
    }

    #[test]
    fn sha256_matches_known_value() {
        let p = std::env::temp_dir().join(format!("tsukimi-sha-{}", rand::random::<u32>()));
        std::fs::write(&p, b"abc").unwrap();
        let h = sha256_file(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
