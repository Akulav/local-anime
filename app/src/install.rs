//! Installing on Linux and macOS (Windows: the Inno Setup wizard, app/installer/tsukimi.iss).
//!
//! - Linux: the release file `tsukimi-linux-x64` is the whole program. Started from anywhere
//!   (Downloads…) it copies itself to ~/.local/share/tsukimi/tsukimi, adds an app-menu entry (and a
//!   desktop shortcut when there is a desktop folder) and starts that copy. `--here` runs where it is
//!   instead, `--uninstall` removes the program and its entries (the library, settings and downloads
//!   only with `--delete-data`).
//! - macOS: the release file `tsukimi-macos-arm64` is the whole program too. Started from anywhere it
//!   builds ~/Applications/Tsukimi.app around a copy of itself (Info.plist, icon, ad-hoc signature)
//!   and opens that. `--here` runs where it is; `--uninstall [--delete-data]` removes the app.
//!
//! So a release has three programs, one per system, and each both installs and updates
//! (Windows: `Tsukimi-setup.exe`, see updater.rs).
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
    #[cfg(target_os = "linux")]
    {
        if has("--here") {
            return true;
        }
        let Ok(exe) = std::env::current_exe() else { return true };
        if tsukimi_core::updater::self_built(&exe) {
            return true;
        }
        let Some(dir) = install_dir() else { return true };
        let target = dir.join("tsukimi");
        if same_file(&exe, &target) {
            // the installed copy: keep its menu entry in place (it may have been deleted by hand)
            let _ = menu_entries(&target);
            return true;
        }
        match install(&exe, &target) {
            Ok(()) => {
                let rest: Vec<&String> = args.iter().filter(|a| *a != "--after-update").collect();
                match std::process::Command::new(&target).args(rest).spawn() {
                    Ok(_) => {
                        println!("Tsukimi is installed in {} and has an app menu entry.", dir.display());
                        false
                    }
                    Err(e) => {
                        eprintln!(
                            "Tsukimi: installed, but could not start {}: {e}. Running from here.",
                            target.display()
                        );
                        true
                    }
                }
            }
            Err(e) => {
                eprintln!("Tsukimi: could not install into {} ({e}). Running from here.", dir.display());
                true
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if has("--here") {
            return true;
        }
        let Ok(exe) = std::env::current_exe() else { return true };
        if tsukimi_core::updater::self_built(&exe) || tsukimi_core::updater::app_bundle(&exe).is_some() {
            return true; // built here, or already running inside an .app
        }
        let Some(app) = mac_app_dir() else { return true };
        match mac_install(&exe, &app) {
            Ok(()) => match std::process::Command::new("/usr/bin/open").arg(&app).spawn() {
                Ok(_) => {
                    println!("Tsukimi is installed as {}.", app.display());
                    false
                }
                Err(e) => {
                    eprintln!("Tsukimi: installed, but could not open {}: {e}. Running from here.", app.display());
                    true
                }
            },
            Err(e) => {
                eprintln!("Tsukimi: could not install into {} ({e}). Running from here.", app.display());
                true
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    true
}

/// ~/Applications/Tsukimi.app (a per-user Applications folder: no administrator password).
#[cfg(target_os = "macos")]
fn mac_app_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Applications").join("Tsukimi.app"))
}

/// Builds (or refreshes) the app bundle around a copy of this program, unless a newer one is there.
#[cfg(target_os = "macos")]
fn mac_install(exe: &Path, app: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    const ICNS: &[u8] = include_bytes!("../icons/icon.icns");
    const PLIST: &str = include_str!("../installer/Info.plist");
    let macos = app.join("Contents").join("MacOS");
    let res = app.join("Contents").join("Resources");
    std::fs::create_dir_all(&macos)?;
    std::fs::create_dir_all(&res)?;
    let bin = macos.join("tsukimi");
    let mine = tsukimi_core::VERSION;
    let keep_installed = installed_version(&bin).is_some_and(|v| !tsukimi_core::updater::newer(mine, &v));
    if !keep_installed {
        let tmp = bin.with_extension("new");
        std::fs::copy(exe, &tmp)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, &bin)?;
        std::fs::write(res.join("icon.icns"), ICNS)?;
        std::fs::write(app.join("Contents").join("Info.plist"), PLIST.replace("@VERSION@", mine))?;
        // ad hoc: no certificate; macOS asks once on the first open (right-click → Open)
        let _ = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--deep", "--sign", "-"])
            .arg(app)
            .status();
    }
    Ok(())
}

/// ~/.local/share/tsukimi (`$XDG_DATA_HOME/tsukimi`), which is also where the library lives.
#[cfg(target_os = "linux")]
fn install_dir() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home().map(|h| h.join(".local").join("share")))?;
    Some(data.join("tsukimi"))
}

#[cfg(target_os = "linux")]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(target_os = "linux")]
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The version an installed program reports (`tsukimi --version` → "Tsukimi 4.1.0 (made by Akulav)").
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn installed_version(target: &Path) -> Option<String> {
    let out = std::process::Command::new(target).arg("--version").output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1).map(String::from)
}

/// Copies this program into place (unless a newer one is there already) and adds the menu entries.
#[cfg(target_os = "linux")]
fn install(exe: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    std::fs::create_dir_all(dir)?;
    let mine = tsukimi_core::VERSION;
    let keep_installed = installed_version(target).is_some_and(|v| !tsukimi_core::updater::newer(mine, &v));
    if !keep_installed {
        // write beside it and rename: replacing a running program in place fails ("text file busy")
        let tmp = target.with_extension("new");
        std::fs::copy(exe, &tmp)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, target)?;
    }
    menu_entries(target)
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
    if let Some(data) = install_dir().and_then(|d| d.parent().map(Path::to_path_buf)) {
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
    #[cfg(target_os = "linux")]
    {
        let Some(dir) = install_dir() else { return };
        for p in entry_paths() {
            let _ = std::fs::remove_file(p);
        }
        for f in ["tsukimi", "tsukimi.png", "tsukimi.old", "tsukimi.new"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        if delete_data {
            let _ = std::fs::remove_dir_all(&dir);
            if let Some(cache) = home().map(|h| h.join(".cache").join("tsukimi")) {
                let _ = std::fs::remove_dir_all(cache);
            }
            println!("Tsukimi and its data (library, settings, downloads, cache) were removed.");
        } else {
            let _ = std::fs::remove_dir(&dir); // only when empty
            println!(
                "Tsukimi was removed. Your library, settings and downloads stay in {} (run with --uninstall --delete-data to remove them too).",
                dir.display()
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(app) = mac_app_dir().filter(|a| a.exists()) {
            let _ = std::fs::remove_dir_all(&app);
            println!("Removed {}.", app.display());
        }
        if delete_data {
            if let Ok(p) = tsukimi_core::Paths::resolve() {
                for d in [&p.data, &p.cache, &p.downloads] {
                    let _ = std::fs::remove_dir_all(d);
                }
                println!("Tsukimi's library, settings, downloads and cache were removed.");
            }
        } else {
            let data = tsukimi_core::Paths::resolve().map(|p| p.data.display().to_string()).unwrap_or_default();
            println!("Your library, settings and downloads stay in {data} (add --delete-data to remove them).");
        }
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
}
