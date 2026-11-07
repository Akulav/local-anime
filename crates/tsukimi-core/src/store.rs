//! SQLite-backed persistence: library, watch progress, settings and a TTL key/value cache.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub struct Store {
    conn: Mutex<Connection>,
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub anilist_id: i64,
    pub episode: String,
    pub position: f64,
    pub duration: f64,
    pub updated_at: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    pub anilist_id: i64,
    pub title: String,
    pub cover: Option<String>,
    pub added_at: i64,
    pub status: String,
}

/// A downloaded (or queued) episode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRecord {
    pub key: String,
    pub anilist_id: i64,
    pub episode: String,
    pub mode: String,
    pub title: String,
    pub cover: Option<String>,
    pub provider: String,
    /// Saved by the user: kept until deleted instead of expiring.
    pub pinned: bool,
    /// JSON [`crate::stream::SavedSource`]; empty while the episode is still queued.
    #[serde(skip)]
    pub source: Option<String>,
    pub total: u64,
    pub done: u64,
    pub bytes: u64,
    pub error: Option<String>,
    pub created_at: i64,
    pub used_at: i64,
}

const DOWNLOAD_COLS: &str =
    "key, anilist_id, episode, mode, title, cover, provider, pinned, source, total, done, bytes, error, created_at, used_at";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA temp_store=MEMORY;
            CREATE TABLE IF NOT EXISTS kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                expires_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS library (
                anilist_id INTEGER PRIMARY KEY,
                title TEXT NOT NULL,
                cover TEXT,
                added_at INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'watching'
            );
            CREATE TABLE IF NOT EXISTS progress (
                anilist_id INTEGER NOT NULL,
                episode TEXT NOT NULL,
                position REAL NOT NULL,
                duration REAL NOT NULL,
                updated_at INTEGER NOT NULL,
                title TEXT,
                cover TEXT,
                provider TEXT,
                PRIMARY KEY (anilist_id, episode)
            );
            CREATE INDEX IF NOT EXISTS progress_recent ON progress(updated_at DESC);
            CREATE TABLE IF NOT EXISTS downloads (
                key TEXT PRIMARY KEY,
                anilist_id INTEGER NOT NULL,
                episode TEXT NOT NULL,
                mode TEXT NOT NULL,
                title TEXT NOT NULL,
                cover TEXT,
                provider TEXT NOT NULL,
                pinned INTEGER NOT NULL DEFAULT 0,
                source TEXT,
                total INTEGER NOT NULL DEFAULT 0,
                done INTEGER NOT NULL DEFAULT 0,
                bytes INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                created_at INTEGER NOT NULL,
                used_at INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    // ---- kv cache ----
    pub fn kv_get(&self, key: &str) -> Option<String> {
        let c = self.conn.lock();
        c.query_row("SELECT value FROM kv WHERE key = ?1 AND expires_at > ?2", params![key, now_secs()], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn kv_set(&self, key: &str, value: &str, ttl_secs: i64) {
        let c = self.conn.lock();
        let _ = c.execute(
            "INSERT INTO kv (key, value, expires_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at",
            params![key, value, now_secs() + ttl_secs.max(1)],
        );
    }

    pub fn kv_purge(&self) -> usize {
        let c = self.conn.lock();
        c.execute("DELETE FROM kv WHERE expires_at <= ?1", params![now_secs()]).unwrap_or(0)
    }

    pub fn kv_clear(&self) {
        let c = self.conn.lock();
        let _ = c.execute("DELETE FROM kv", []);
    }

    // ---- settings ----
    pub fn setting(&self, key: &str) -> Option<String> {
        let c = self.conn.lock();
        c.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- library ----
    pub fn library(&self) -> Result<Vec<LibraryEntry>> {
        let c = self.conn.lock();
        let mut st =
            c.prepare("SELECT anilist_id, title, cover, added_at, status FROM library ORDER BY added_at DESC")?;
        let rows = st.query_map([], |r| {
            Ok(LibraryEntry {
                anilist_id: r.get(0)?,
                title: r.get(1)?,
                cover: r.get(2)?,
                added_at: r.get(3)?,
                status: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn library_add(&self, id: i64, title: &str, cover: Option<&str>) -> Result<()> {
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO library (anilist_id, title, cover, added_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(anilist_id) DO UPDATE SET title = excluded.title, cover = excluded.cover",
            params![id, title, cover, now_secs()],
        )?;
        Ok(())
    }

    pub fn library_remove(&self, id: i64) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM library WHERE anilist_id = ?1", params![id])?;
        Ok(())
    }

    pub fn in_library(&self, id: i64) -> bool {
        let c = self.conn.lock();
        c.query_row("SELECT 1 FROM library WHERE anilist_id = ?1", params![id], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    // ---- progress ----
    pub fn save_progress(&self, p: &Progress) -> Result<()> {
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO progress (anilist_id, episode, position, duration, updated_at, title, cover, provider)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(anilist_id, episode) DO UPDATE SET
               position = excluded.position, duration = excluded.duration, updated_at = excluded.updated_at,
               title = COALESCE(excluded.title, progress.title), cover = COALESCE(excluded.cover, progress.cover),
               provider = COALESCE(excluded.provider, progress.provider)",
            params![p.anilist_id, p.episode, p.position, p.duration, now_secs(), p.title, p.cover, p.provider],
        )?;
        Ok(())
    }

    fn map_progress(r: &rusqlite::Row) -> rusqlite::Result<Progress> {
        Ok(Progress {
            anilist_id: r.get(0)?,
            episode: r.get(1)?,
            position: r.get(2)?,
            duration: r.get(3)?,
            updated_at: r.get(4)?,
            title: r.get(5)?,
            cover: r.get(6)?,
            provider: r.get(7)?,
        })
    }

    pub fn progress_for(&self, id: i64) -> Result<Vec<Progress>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT anilist_id, episode, position, duration, updated_at, title, cover, provider
             FROM progress WHERE anilist_id = ?1",
        )?;
        let rows = st.query_map(params![id], Self::map_progress)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Most recent episode per show, newest first.
    pub fn continue_watching(&self, limit: usize) -> Result<Vec<Progress>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT p.anilist_id, p.episode, p.position, p.duration, p.updated_at, p.title, p.cover, p.provider
             FROM progress p
             JOIN (SELECT anilist_id, MAX(updated_at) AS m FROM progress GROUP BY anilist_id) last
               ON last.anilist_id = p.anilist_id AND last.m = p.updated_at
             ORDER BY p.updated_at DESC LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit as i64], Self::map_progress)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn remove_progress(&self, id: i64) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM progress WHERE anilist_id = ?1", params![id])?;
        Ok(())
    }

    // ---- downloads ----
    fn map_download(r: &rusqlite::Row) -> rusqlite::Result<DownloadRecord> {
        Ok(DownloadRecord {
            key: r.get(0)?,
            anilist_id: r.get(1)?,
            episode: r.get(2)?,
            mode: r.get(3)?,
            title: r.get(4)?,
            cover: r.get(5)?,
            provider: r.get(6)?,
            pinned: r.get(7)?,
            source: r.get(8)?,
            total: r.get::<_, i64>(9)? as u64,
            done: r.get::<_, i64>(10)? as u64,
            bytes: r.get::<_, i64>(11)? as u64,
            error: r.get(12)?,
            created_at: r.get(13)?,
            used_at: r.get(14)?,
        })
    }

    pub fn downloads(&self) -> Result<Vec<DownloadRecord>> {
        let c = self.conn.lock();
        let mut st = c.prepare(&format!("SELECT {DOWNLOAD_COLS} FROM downloads ORDER BY created_at, rowid"))?;
        let rows = st.query_map([], Self::map_download)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn download(&self, key: &str) -> Option<DownloadRecord> {
        let c = self.conn.lock();
        c.query_row(&format!("SELECT {DOWNLOAD_COLS} FROM downloads WHERE key = ?1"), params![key], Self::map_download)
            .optional()
            .ok()
            .flatten()
    }

    /// Inserts a queued download; an existing one keeps its data and only gains `pinned`.
    pub fn download_add(&self, d: &DownloadRecord) -> Result<()> {
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO downloads (key, anilist_id, episode, mode, title, cover, provider, pinned, created_at, used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
             ON CONFLICT(key) DO UPDATE SET pinned = MAX(downloads.pinned, excluded.pinned), error = NULL",
            params![d.key, d.anilist_id, d.episode, d.mode, d.title, d.cover, d.provider, d.pinned, now_secs()],
        )?;
        Ok(())
    }

    pub fn download_set_source(&self, key: &str, provider: &str, source: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute(
            "UPDATE downloads SET source = ?2, provider = ?3, error = NULL WHERE key = ?1",
            params![key, source, provider],
        )?;
        Ok(())
    }

    pub fn download_progress(&self, key: &str, total: u64, done: u64, bytes: u64) {
        let c = self.conn.lock();
        let _ = c.execute(
            "UPDATE downloads SET total = ?2, done = ?3, bytes = ?4 WHERE key = ?1",
            params![key, total as i64, done as i64, bytes as i64],
        );
    }

    pub fn download_error(&self, key: &str, error: &str) {
        let c = self.conn.lock();
        let _ = c.execute("UPDATE downloads SET error = ?2 WHERE key = ?1", params![key, error]);
    }

    pub fn download_clear_error(&self, key: &str) {
        let c = self.conn.lock();
        let _ = c.execute("UPDATE downloads SET error = NULL WHERE key = ?1", params![key]);
    }

    pub fn download_touch(&self, key: &str) {
        let c = self.conn.lock();
        let _ = c.execute("UPDATE downloads SET used_at = ?2 WHERE key = ?1", params![key, now_secs()]);
    }

    pub fn download_pin(&self, key: &str, pinned: bool) -> Result<()> {
        let c = self.conn.lock();
        c.execute("UPDATE downloads SET pinned = ?2, used_at = ?3 WHERE key = ?1", params![key, pinned, now_secs()])?;
        Ok(())
    }

    pub fn download_delete(&self, key: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM downloads WHERE key = ?1", params![key])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_keep_data_and_only_gain_pin() {
        let path = std::env::temp_dir().join(format!("tsukimi-store-{}.db", rand::random::<u32>()));
        let s = Store::open(&path).unwrap();
        let mut d = DownloadRecord {
            key: "k".into(),
            anilist_id: 1,
            episode: "3".into(),
            mode: "sub".into(),
            title: "Show".into(),
            cover: None,
            provider: "p".into(),
            pinned: false,
            source: None,
            total: 0,
            done: 0,
            bytes: 0,
            error: None,
            created_at: 0,
            used_at: 0,
        };
        s.download_add(&d).unwrap();
        s.download_set_source("k", "p2", "{}").unwrap();
        s.download_progress("k", 10, 4, 400);
        s.download_error("k", "boom");
        d.pinned = true;
        s.download_add(&d).unwrap();
        let r = s.download("k").unwrap();
        assert!(r.pinned && r.error.is_none());
        assert_eq!((r.provider.as_str(), r.source.as_deref(), r.done, r.total), ("p2", Some("{}"), 4, 10));
        d.pinned = false;
        s.download_add(&d).unwrap();
        assert!(s.download("k").unwrap().pinned, "re-adding never unpins");
        s.download_delete("k").unwrap();
        assert!(s.downloads().unwrap().is_empty());
        drop(s);
        let _ = std::fs::remove_file(&path);
    }
}
