//! Installing on Linux and macOS (Windows: the Inno Setup wizard, app/installer/tsukimi.iss).
//!
//! The release files `tsukimi-linux-x64` / `tsukimi-macos-arm64` are the whole program, and each both
//! installs and updates (Windows: `Tsukimi-setup.exe`, see updater.rs). Started from anywhere
//! (Downloads…) the program asks for an **install folder** with the system's folder picker (zenity /
//! kdialog on Linux, Finder on macOS; `--install-dir <folder>` skips the question), copies itself there
//! and starts that copy. The library, settings, cache and downloads then live in `<folder>/data`.
//!
//! - Linux: `<folder>/tsukimi` (default `~/.local/share/tsukimi`) + an app-menu entry (and a desktop
//!   shortcut when there is a desktop folder).
//! - macOS: `<folder>/Tsukimi.app` (default `~/Applications/Tsukimi`; Info.plist, icon, ad-hoc signature).
//!
//! The folder is remembered (`<config dir>/tsukimi/install-dir`), so a newer download updates that
//! copy without asking. `--here` runs where it is; `--uninstall [--delete-data]` removes the program
//! (and with `--delete-data` its `data` folder).
//!
//! A program built on this computer (cargo / tauri: …/target/…) always runs in place.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::{Path, PathBuf};

/// Returns true when this process should go on and start Tsukimi, false when it has done its job.
pub fn first_run() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x == a);
    if has("--uninstall") {
        uninstall(has("--delete-data"));
        return false;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if has("--here") {
            return true;
        }
        let Ok(exe) = std::env::current_exe() else { return true };
        if tsukimi_core::updater::self_built(&exe) {
            return true;
        }
        if let Some(root) = tsukimi_core::program_root().filter(|r| tsukimi_core::is_install_root(r)) {
            // the installed copy: keep its marker, menu entry and remembered folder in place
            on_installed_start(&root);
            return true;
        }
        // Versions before 4.2 installed into a fixed place: adopt or move that install.
        if let Some(root) = adopt_legacy_install(&exe) {
            on_installed_start(&root);
            return true;
        }
        let explicit =
            args.iter().position(|a| a == "--install-dir").and_then(|i| args.get(i + 1)).map(|d| expand_home(d));
        let folder = match explicit.or_else(remembered_folder) {
            Some(f) => f,
            None => match ask_folder(&default_folder()) {
                Some(f) => f,
                None => return true, // cancelled: just run it
            },
        };
        match install(&exe, &folder) {
            Ok(()) => {
                let rest: Vec<&String> = args.iter().filter(|a| *a != "--after-update").collect();
                match launch(&folder, &rest) {
                    Ok(()) => {
                        println!("Tsukimi is installed in {}.", folder.display());
                        false
                    }
                    Err(e) => {
                        eprintln!("Tsukimi: installed, but could not start it: {e}. Running from here.");
                        true
                    }
                }
            }
            Err(e) => {
                eprintln!("Tsukimi: could not install into {} ({e}). Running from here.", folder.display());
                true
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    true
}

// ---------------------------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn expand_home(text: &str) -> PathBuf {
    match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home().unwrap_or_default().join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(text),
    }
}

/// `~/.local/share/tsukimi` (`$XDG_DATA_HOME/tsukimi`) on Linux, `~/Applications/Tsukimi` on macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn default_folder() -> PathBuf {
    if cfg!(target_os = "macos") {
        return home().unwrap_or_default().join("Applications").join("Tsukimi");
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().unwrap_or_default().join(".local").join("share"))
        .join("tsukimi")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pointer_file() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("tsukimi").join("install-dir"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remembered_folder() -> Option<PathBuf> {
    let t = std::fs::read_to_string(pointer_file()?).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| PathBuf::from(t))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remember(folder: &Path) {
    if let Some(f) = pointer_file() {
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(f, folder.to_string_lossy().as_bytes());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The program inside an install folder.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn program_in(folder: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        folder.join("Tsukimi.app").join("Contents").join("MacOS").join("tsukimi")
    } else {
        folder.join("tsukimi")
    }
}

/// The system's folder picker; `None` when cancelled. Without one, the default folder is used.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ask_folder(default: &Path) -> Option<PathBuf> {
    use std::process::Command;
    let _ = std::fs::create_dir_all(default.parent().unwrap_or(default));
    let title = "Choose where to install Tsukimi (your library and downloads go there too)";
    let out = if cfg!(target_os = "macos") {
        let start = default.parent().unwrap_or(default).display().to_string().replace('"', "");
        Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!(
                "POSIX path of (choose folder with prompt \"{title}. A Tsukimi folder is created inside the one you pick.\" default location POSIX file \"{start}\")"
            ))
            .output()
            .ok()
    } else {
        Command::new("zenity")
            .args(["--file-selection", "--directory", "--title", title, "--filename"])
            .arg(format!("{}/", default.display()))
            .output()
            .ok()
            .or_else(|| {
                Command::new("kdialog")
                    .args(["--getexistingdirectory"])
                    .arg(default)
                    .args(["--title", title])
                    .output()
                    .ok()
            })
    };
    match out {
        // no picker on this system: the default folder
        None => Some(default.to_path_buf()),
        Some(o) if o.status.success() => {
            let picked = String::from_utf8_lossy(&o.stdout).trim().trim_end_matches('/').to_string();
            if picked.is_empty() {
                return None;
            }
            let p = PathBuf::from(picked);
            // macOS picks the parent (Finder can't name a new folder here): install into <it>/Tsukimi
            Some(if cfg!(target_os = "macos") && !p.ends_with("Tsukimi") { p.join("Tsukimi") } else { p })
        }
        Some(_) => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn on_installed_start(root: &Path) {
    if std::fs::read_to_string(root.join(tsukimi_core::INSTALL_MARKER)).ok().as_deref().map(str::trim)
        != Some(tsukimi_core::VERSION)
    {
        let _ = std::fs::write(root.join(tsukimi_core::INSTALL_MARKER), tsukimi_core::VERSION);
    }
    if remembered_folder().is_none_or(|r| !same_file(&r, root)) {
        remember(root);
    }
    #[cfg(target_os = "linux")]
    let _ = menu_entries(&program_in(root));
}

/// Linux before 4.2: `~/.local/share/tsukimi/tsukimi` (updated in place) — that folder simply becomes
/// the install folder. macOS before 4.2: `~/Applications/Tsukimi.app` — moved into
/// `~/Applications/Tsukimi/` (this process then starts the moved copy and ends).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn adopt_legacy_install(exe: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        let d = default_folder();
        if same_file(exe, &d.join("tsukimi")) {
            return Some(d);
        }
        return None;
    }
    let old_app = home()?.join("Applications").join("Tsukimi.app");
    if !exe.starts_with(&old_app) && !exe.canonicalize().ok()?.starts_with(old_app.canonicalize().ok()?) {
        return None;
    }
    let folder = remembered_folder().unwrap_or_else(default_folder);
    if install(exe, &folder).is_ok() && launch(&folder, &[]).is_ok() {
        let _ = std::fs::remove_dir_all(&old_app);
        std::process::exit(0);
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn launch(folder: &Path, args: &[&String]) -> std::io::Result<()> {
    if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open").arg("-n").arg(folder.join("Tsukimi.app")).spawn()?;
    } else {
        std::process::Command::new(program_in(folder)).args(args).current_dir(folder).spawn()?;
    }
    Ok(())
}

/// The version an installed program reports (`tsukimi --version` → "Tsukimi 4.1.0 (made by Akulav)").
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn installed_version(target: &Path) -> Option<String> {
    let out = std::process::Command::new(target).arg("--version").output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1).map(String::from)
}

/// Copies this program into `folder` (unless a newer one is there already), marks and remembers the
/// folder and adds the menu entry / app bundle.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn install(exe: &Path, folder: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let target = program_in(folder);
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    std::fs::create_dir_all(dir)?;
    let mine = tsukimi_core::VERSION;
    let keep_installed = installed_version(&target).is_some_and(|v| !tsukimi_core::updater::newer(mine, &v));
    if !keep_installed && !same_file(exe, &target) {
        // write beside it and rename: replacing a running program in place fails ("text file busy")
        let tmp = target.with_extension("new");
        std::fs::copy(exe, &tmp)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, &target)?;
    }
    #[cfg(target_os = "macos")]
    {
        const ICNS: &[u8] = include_bytes!("../icons/icon.icns");
        const PLIST: &str = include_str!("../installer/Info.plist");
        let app = folder.join("Tsukimi.app");
        let res = app.join("Contents").join("Resources");
        std::fs::create_dir_all(&res)?;
        std::fs::write(res.join("icon.icns"), ICNS)?;
        std::fs::write(app.join("Contents").join("Info.plist"), PLIST.replace("@VERSION@", mine))?;
        let _ = std::process::Command::new("xattr").args(["-dr", "com.apple.quarantine"]).arg(&app).status();
        // ad hoc: no certificate; macOS asks once on the first open (right-click → Open)
        let _ = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--deep", "--sign", "-"])
            .arg(&app)
            .status();
    }
    std::fs::write(folder.join(tsukimi_core::INSTALL_MARKER), mine)?;
    remember(folder);
    #[cfg(target_os = "linux")]
    menu_entries(&target)?;
    Ok(())
}

/// 256 px app icon for the menu entry.
#[cfg(target_os = "linux")]
const ICON_PNG: &[u8] = include_bytes!("../icons/128x128@2x.png");

#[cfg(target_os = "linux")]
fn desktop_file(target: &Path, icon: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Tsukimi\nGenericName=Anime player\n\
         Comment=Fast desktop anime player - made by Akulav\n\
         Exec=\"{}\"\nIcon={}\nTerminal=false\nCategories=AudioVideo;Video;Player;\nStartupWMClass=tsukimi\n",
        target.display(),
        icon.display()
    )
}

/// ~/.local/share/applications/tsukimi.desktop, plus ~/Desktop/tsukimi.desktop when a desktop
/// folder exists.
#[cfg(target_os = "linux")]
fn entry_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let share = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home().map(|h| h.join(".local").join("share")));
    if let Some(data) = share {
        v.push(data.join("applications").join("tsukimi.desktop"));
    }
    if let Some(desk) = home().map(|h| h.join("Desktop")).filter(|d| d.is_dir()) {
        v.push(desk.join("tsukimi.desktop"));
    }
    v
}

#[cfg(target_os = "linux")]
fn menu_entries(target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    let icon = dir.join("tsukimi.png");
    if std::fs::read(&icon).ok().as_deref() != Some(ICON_PNG) {
        std::fs::write(&icon, ICON_PNG)?;
    }
    let text = desktop_file(target, &icon);
    for p in entry_paths() {
        if std::fs::read_to_string(&p).ok().as_deref() == Some(text.as_str()) {
            continue;
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, &text)?;
        // a desktop launcher must be executable to be trusted
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn uninstall(delete_data: bool) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let Some(dir) = remembered_folder().or_else(|| {
            let d = default_folder();
            program_in(&d).is_file().then_some(d)
        }) else {
            println!("Tsukimi is not installed for this user.");
            return;
        };
        #[cfg(target_os = "linux")]
        {
            for p in entry_paths() {
                let _ = std::fs::remove_file(p);
            }
            for f in ["tsukimi", "tsukimi.png", "tsukimi.old", "tsukimi.new"] {
                let _ = std::fs::remove_file(dir.join(f));
            }
        }
        #[cfg(target_os = "macos")]
        let _ = std::fs::remove_dir_all(dir.join("Tsukimi.app"));
        let _ = std::fs::remove_file(dir.join(tsukimi_core::INSTALL_MARKER));
        if let Some(f) = pointer_file() {
            let _ = std::fs::remove_file(f);
        }
        if delete_data {
            let _ = std::fs::remove_dir_all(dir.join("data"));
            println!("Tsukimi and its data (library, settings, downloads, cache) were removed.");
        } else {
            println!(
                "Tsukimi was removed. Your library, settings and downloads stay in {} (run with --uninstall --delete-data to remove them too).",
                dir.join("data").display()
            );
        }
        let _ = std::fs::remove_dir(&dir); // only when empty
    }
    #[cfg(windows)]
    {
        let _ = delete_data;
        println!("Uninstall Tsukimi from Settings -> Apps (or Start menu -> Uninstall Tsukimi).");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn menu_entry_points_at_the_installed_program() {
        let t = desktop_file(
            Path::new("/home/u/.local/share/tsukimi/tsukimi"),
            Path::new("/home/u/.local/share/tsukimi/tsukimi.png"),
        );
        assert!(t.contains("Exec=\"/home/u/.local/share/tsukimi/tsukimi\""));
        assert!(t.contains("Icon=/home/u/.local/share/tsukimi/tsukimi.png"));
        assert!(t.starts_with("[Desktop Entry]"));
    }

    #[test]
    fn programs_live_in_the_install_folder() {
        assert_eq!(program_in(Path::new("/srv/t")), PathBuf::from("/srv/t/tsukimi"));
        assert!(expand_home("~/x").ends_with("x"));
    }
}
