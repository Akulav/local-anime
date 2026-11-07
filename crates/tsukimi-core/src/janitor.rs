//! Disk cache housekeeping: age-based expiry plus LRU eviction down to the size limit.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub bytes: u64,
    pub files: u64,
    pub segment_bytes: u64,
    pub file_bytes: u64,
    pub image_bytes: u64,
    /// Saved downloads (the downloads folder, not part of the cache or its limit).
    pub download_bytes: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepReport {
    pub removed: u64,
    pub freed: u64,
    pub remaining: u64,
}

struct Entry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn walk(dir: &Path, out: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            walk(&e.path(), out);
        } else {
            out.push(Entry {
                path: e.path(),
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
}

/// Total size of the files under `dir`.
pub fn dir_size(dir: &Path) -> u64 {
    let mut v = Vec::new();
    walk(dir, &mut v);
    v.iter().map(|e| e.size).sum()
}

pub fn stats(cache_dir: &Path) -> CacheStats {
    let mut s = CacheStats::default();
    for (sub, slot) in [("segments", 0), ("files", 1), ("images", 2)] {
        let mut v = Vec::new();
        walk(&cache_dir.join(sub), &mut v);
        let bytes: u64 = v.iter().map(|e| e.size).sum();
        s.files += v.len() as u64;
        s.bytes += bytes;
        match slot {
            0 => s.segment_bytes = bytes,
            1 => s.file_bytes = bytes,
            _ => s.image_bytes = bytes,
        }
    }
    s
}

/// Deletes expired entries, then the least recently used ones until the cache is at 90% of
/// `limit`. Images live longer (30 days) and are evicted last. `keep` files (running and saved
/// downloads) are skipped.
pub fn sweep(cache_dir: &Path, limit: u64, max_age: Duration, keep: &HashSet<PathBuf>) -> SweepReport {
    let now = SystemTime::now();
    let mut media = Vec::new();
    walk(&cache_dir.join("segments"), &mut media);
    walk(&cache_dir.join("files"), &mut media);
    let mut images = Vec::new();
    walk(&cache_dir.join("images"), &mut images);

    let mut report = SweepReport::default();
    let remove = |e: &Entry, report: &mut SweepReport| {
        if keep.contains(&e.path) {
            return false;
        }
        if std::fs::remove_file(&e.path).is_ok() {
            report.removed += 1;
            report.freed += e.size;
            true
        } else {
            false
        }
    };
    let age = |e: &Entry| now.duration_since(e.modified).unwrap_or_default();

    let mut keep_media = Vec::new();
    for e in media {
        // Stray temp files from interrupted writes.
        let stale_tmp = e.path.extension().is_some_and(|x| x == "tmp") && age(&e) > Duration::from_secs(600);
        if (age(&e) > max_age || stale_tmp) && remove(&e, &mut report) {
            continue;
        }
        keep_media.push(e);
    }
    let mut keep_images = Vec::new();
    for e in images {
        if age(&e) > Duration::from_secs(30 * 86_400) && remove(&e, &mut report) {
            continue;
        }
        keep_images.push(e);
    }

    let mut total: u64 = keep_media.iter().chain(keep_images.iter()).map(|e| e.size).sum();
    if total > limit {
        let target = limit / 10 * 9;
        keep_media.sort_by_key(|e| e.modified);
        keep_images.sort_by_key(|e| e.modified);
        for e in keep_media.iter().chain(keep_images.iter()) {
            if total <= target {
                break;
            }
            if remove(e, &mut report) {
                total = total.saturating_sub(e.size);
            }
        }
    }
    report.remaining = total;
    report
}

/// Removes every cached stream file (images too when `images` is set).
pub fn clear(cache_dir: &Path, images: bool, keep: &HashSet<PathBuf>) -> SweepReport {
    let mut v = Vec::new();
    walk(&cache_dir.join("segments"), &mut v);
    walk(&cache_dir.join("files"), &mut v);
    if images {
        walk(&cache_dir.join("images"), &mut v);
    }
    let mut r = SweepReport::default();
    for e in v {
        if keep.contains(&e.path) {
            continue;
        }
        if std::fs::remove_file(&e.path).is_ok() {
            r.removed += 1;
            r.freed += e.size;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_until_under_limit() {
        let dir = std::env::temp_dir().join(format!("tsukimi-janitor-{}", rand::random::<u32>()));
        let seg = dir.join("segments").join("ab");
        std::fs::create_dir_all(&seg).unwrap();
        for i in 0..10u64 {
            let p = seg.join(format!("f{i}"));
            std::fs::write(&p, vec![0u8; 1000]).unwrap();
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(100 - i)).unwrap();
        }
        let keep = HashSet::from([seg.join("f0")]);
        let r = sweep(&dir, 5000, Duration::from_secs(3600), &keep);
        assert!(r.remaining <= 5000, "{r:?}");
        assert!(seg.join("f0").exists(), "busy file kept");
        assert!(seg.join("f9").exists(), "newest kept");
        assert!(!seg.join("f1").exists(), "oldest evicted");
        let r = sweep(&dir, 1 << 30, Duration::from_secs(1), &HashSet::new());
        assert!(r.remaining == 0 && r.removed > 0, "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
