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
}
