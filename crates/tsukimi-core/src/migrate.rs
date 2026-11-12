//! One-off moves from the app's previous name (dango) so an existing library, watch progress,
//! settings and downloads carry over to Tsukimi. Every step is best effort: a failure leaves the old
//! files where they were and Tsukimi starts fresh.

use std::path::Path;

use crate::Paths;

/// Tsukimi before 4.2 kept its data in the OS's per-user folders: move the database, logs, provider
/// modules, cache and downloads into the install folder's `data/`. Only Tsukimi's own entries move
/// (on Linux the old data folder was also the install folder, so the program sits there too), and
/// nothing that already exists at the destination is overwritten.
pub(crate) fn from_os_folders(new: &Paths, old_data: &Path, old_cache: &Path, old_downloads: &Path) {
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    };
    let move_one = |from: &Path, to: &Path| {
        if !from.exists() || to.exists() || same(from, to) || to.starts_with(from) {
            return;
        }
        let r = if from.is_dir() { move_dir(from, to) } else { move_file(from, to) };
        match r {
            Ok(()) => tracing::info!("moved {} to {}", from.display(), to.display()),
            Err(e) => tracing::warn!("could not move {} to {}: {e}", from.display(), to.display()),
        }
    };
    if let Ok(rd) = std::fs::read_dir(old_data) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let ours = name.starts_with("tsukimi.db")
                || name.starts_with("tsukimi.log")
                || name == "crash.log"
                || name == "remote-providers";
            if ours {
                move_one(&e.path(), &new.data.join(&name));
            }
        }
    }
    let _ = std::fs::remove_dir_all(old_data.join("updates"));
    move_one(old_downloads, &new.downloads);
    move_one(old_cache, &new.cache);
    // The webview's own data (Tauri's default `<local data>/com.akulav.tsukimi`).
    if let Some(d) = directories::BaseDirs::new() {
        move_one(&d.data_local_dir().join("com.akulav.tsukimi"), &new.data.join("webview"));
    }
    // Empty leftovers (`akulav\Tsukimi\data`, …), never anything with files in it.
    for dir in [old_downloads, old_data, old_cache] {
        for p in dir.ancestors().take(3) {
            if std::fs::remove_dir(p).is_err() {
                break;
            }
        }
    }
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// Moves dango's data and cache folders to Tsukimi's, when Tsukimi has none yet.
pub(crate) fn from_dango(new: &Paths, old_data: &Path, old_cache: &Path) {
    for (old, new) in [(old_data, new.data.as_path()), (old_cache, new.cache.as_path())] {
        if new.exists() || !old.is_dir() {
            continue;
        }
        match move_dir(old, new) {
            Ok(()) => {
                tracing::info!("moved {} to {}", old.display(), new.display());
                // dango's ProjectDirs left a "dango" parent folder (Windows: …\dango\dango\data)
                for parent in old.ancestors().skip(1).take(2) {
                    if std::fs::remove_dir(parent).is_err() {
                        break;
                    }
                }
            }
            Err(e) => tracing::warn!("could not move {} to {}: {e}", old.display(), new.display()),
        }
    }
}

/// `dango.db` (and its WAL files) becomes `tsukimi.db` in the same folder.
pub(crate) fn rename_database(dir: &Path, db: &Path) {
    let old = dir.join("dango.db");
    if db.exists() || !old.exists() {
        return;
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = dir.join(format!("dango.db{suffix}"));
        let to = dir.join(format!("tsukimi.db{suffix}"));
        if from.exists() {
            if let Err(e) = std::fs::rename(&from, &to) {
                tracing::warn!("could not rename {}: {e}", from.display());
            }
        }
    }
    let _ = std::fs::remove_file(dir.join("dango.log"));
}

/// Renames a folder, or copies it and removes the original when it is on another drive.
pub(crate) fn move_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    std::fs::remove_dir_all(from)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_dango_data_over() {
        let root = std::env::temp_dir().join(format!("tsukimi-migrate-{}", rand::random::<u32>()));
        let (old_data, old_cache) = (root.join("dango").join("dango").join("data"), root.join("dango-cache"));
        std::fs::create_dir_all(&old_data).unwrap();
        std::fs::create_dir_all(old_cache.join("segments")).unwrap();
        std::fs::write(old_data.join("dango.db"), b"db").unwrap();
        std::fs::write(old_cache.join("segments").join("s"), b"seg").unwrap();
        let new = Paths::under(root.join("tsukimi"));
        from_dango(&new, &old_data, &old_cache);
        rename_database(&new.data, &new.data.join("tsukimi.db"));
        assert_eq!(std::fs::read(new.data.join("tsukimi.db")).unwrap(), b"db");
        assert_eq!(std::fs::read(new.cache.join("segments").join("s")).unwrap(), b"seg");
        assert!(!old_data.exists() && !root.join("dango").exists(), "empty dango folders are removed");
        // a second start changes nothing
        std::fs::create_dir_all(&old_data).unwrap();
        std::fs::write(old_data.join("dango.db"), b"other").unwrap();
        from_dango(&new, &old_data, &old_cache);
        assert_eq!(std::fs::read(new.data.join("tsukimi.db")).unwrap(), b"db");
        let _ = std::fs::remove_dir_all(&root);
    }
}
