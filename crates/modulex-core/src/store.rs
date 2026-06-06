//! The agent state store — a small SQLite DB (`~/.modulex/store.db`) that
//! lets ANY agent register dynamic state through modulex instead of editing
//! config: reminders, countdowns, URL watches, iCal feeds, downstream MCP
//! servers (issue #7).
//!
//! Config describes *structure* (routines, steps); the store holds *state*.
//! Store-backed step types read it at routine time, so anything an agent
//! registers surfaces in the next report automatically.
//!
//! Design rules:
//!
//! - **Causal stamps**: rows carry the engine generation current at the time
//!   of the mutation (`created_gen` / `done_gen` / …) — counters, never
//!   wall-clock. The store also persists the engine's generation counter
//!   (`meta.last_generation`) so generations stay monotonic across process
//!   restarts.
//! - **Sovereignty**: SQLite is operational state, not the record.
//!   [`Store::export_json`] dumps everything as plain JSON; `import_json`
//!   reads it back. No lock-in.
//! - Dates (`due`, `start_date`, …) are display data, never coordination.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Environment variable overriding the store path (first in the search
/// order, before config's `[store] path` and the default).
pub const ENV_STORE: &str = "MODULEX_STORE";

/// Schema version stamped via `PRAGMA user_version`.
const SCHEMA_VERSION: i32 = 1;

/// Errors from store operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Underlying SQLite failure.
    #[error("store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The store directory could not be created.
    #[error("store: cannot create {0}: {1}")]
    Io(PathBuf, std::io::Error),
    /// Import payload malformed.
    #[error("store import: {0}")]
    Import(String),
}

/// A registered reminder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reminder {
    /// Row id.
    pub id: i64,
    /// The reminder text.
    pub text: String,
    /// Optional ISO due date.
    pub due: Option<String>,
    /// Optional recurrence: `daily` | `weekly` | `monthly`.
    pub recurrence: Option<String>,
    /// Engine generation when registered.
    pub created_gen: u64,
    /// Engine generation when marked done (`None` = open).
    pub done_gen: Option<u64>,
}

/// A registered countdown (same display semantics as config countdowns).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredCountdown {
    /// Row id.
    pub id: i64,
    /// Display label.
    pub label: String,
    /// ISO start date.
    pub start_date: String,
    /// ISO end date.
    pub end_date: String,
    /// Denominator for the display template.
    pub total_work_days: u32,
    /// Display template (`{label}`, `{n}`, `{total}`).
    pub display: String,
    /// Engine generation when registered.
    pub created_gen: u64,
    /// Engine generation when retired (`None` = active).
    pub retired_gen: Option<u64>,
}

/// A URL registered for periodic change tracking.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Watch {
    /// Row id.
    pub id: i64,
    /// The http(s) URL.
    pub url: String,
    /// Why it's being watched.
    pub note: String,
    /// Content hash from the last fetch (BLAKE3 hex), if any.
    pub last_hash: Option<String>,
    /// Generation of the last fetch.
    pub last_seen_gen: Option<u64>,
    /// Engine generation when registered.
    pub created_gen: u64,
}

/// Everything in the store, for plain-text export.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoreDump {
    /// Schema version of the dump.
    pub schema_version: i32,
    /// Persisted engine generation.
    pub last_generation: u64,
    /// All reminders (open and done).
    pub reminders: Vec<Reminder>,
    /// All countdowns (active and retired).
    pub countdowns: Vec<StoredCountdown>,
    /// All watches.
    pub watches: Vec<Watch>,
}

/// The store handle. Cheap to share behind an `Arc`; all access serialized
/// through one connection (SQLite is the bottleneck anyway, and routine
/// state traffic is tiny).
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) the store at `path`.
    ///
    /// # Errors
    /// [`StoreError`] when the directory can't be created or SQLite fails.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| StoreError::Io(parent.to_path_buf(), e))?;
            }
        }
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// An in-memory store (tests, ephemeral runs).
    ///
    /// # Errors
    /// [`StoreError`] when SQLite fails.
    pub fn in_memory() -> Result<Self, StoreError> {
        let store = Self {
            conn: Mutex::new(Connection::open_in_memory()?),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Resolve the store path: `$MODULEX_STORE` → `store_path` from config →
    /// `~/.modulex/store.db`.
    #[must_use]
    pub fn resolve_path(config_path: Option<&str>, home: Option<&Path>) -> PathBuf {
        if let Some(env) = std::env::var_os(ENV_STORE) {
            return PathBuf::from(env);
        }
        if let Some(path) = config_path {
            return crate::config::expand_tilde(path);
        }
        home.map_or_else(
            || PathBuf::from("modulex-store.db"),
            |home| home.join(".modulex").join("store.db"),
        )
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS meta (
               key TEXT PRIMARY KEY, value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS reminders (
               id INTEGER PRIMARY KEY,
               text TEXT NOT NULL,
               due TEXT,
               recurrence TEXT,
               created_gen INTEGER NOT NULL,
               done_gen INTEGER
             );
             CREATE TABLE IF NOT EXISTS countdowns (
               id INTEGER PRIMARY KEY,
               label TEXT NOT NULL,
               start_date TEXT NOT NULL,
               end_date TEXT NOT NULL,
               total_work_days INTEGER NOT NULL DEFAULT 30,
               display TEXT NOT NULL,
               created_gen INTEGER NOT NULL,
               retired_gen INTEGER
             );
             CREATE TABLE IF NOT EXISTS watches (
               id INTEGER PRIMARY KEY,
               url TEXT NOT NULL,
               note TEXT NOT NULL DEFAULT '',
               last_hash TEXT,
               last_seen_gen INTEGER,
               created_gen INTEGER NOT NULL
             );
             -- Registered for later phases (issue #7 C/D); schema reserved
             -- now so v1 DBs never need a migration for them.
             CREATE TABLE IF NOT EXISTS ical_feeds (
               id INTEGER PRIMARY KEY,
               source TEXT NOT NULL,
               note TEXT NOT NULL DEFAULT '',
               created_gen INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mcp_servers (
               name TEXT PRIMARY KEY,
               command TEXT NOT NULL,
               args_json TEXT NOT NULL DEFAULT '[]',
               note TEXT NOT NULL DEFAULT '',
               created_gen INTEGER NOT NULL
             );
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
        Ok(())
    }

    // ── generation persistence ─────────────────────────────────────────

    /// The persisted engine generation (0 when never set) — lets the engine
    /// stay monotonic across restarts.
    #[must_use]
    pub fn last_generation(&self) -> u64 {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.query_row(
            "SELECT value FROM meta WHERE key = 'last_generation'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
    }

    /// Persist the engine generation after a run.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn set_last_generation(&self, generation: u64) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('last_generation', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![generation.to_string()],
        )?;
        Ok(())
    }

    // ── reminders ──────────────────────────────────────────────────────

    /// Register a reminder; returns its id.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn reminder_add(
        &self,
        text: &str,
        due: Option<&str>,
        recurrence: Option<&str>,
        generation: u64,
    ) -> Result<i64, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute(
            "INSERT INTO reminders (text, due, recurrence, created_gen) VALUES (?1, ?2, ?3, ?4)",
            params![text, due, recurrence, generation],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Open reminders (not done), oldest first.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn reminders_open(&self) -> Result<Vec<Reminder>, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, text, due, recurrence, created_gen, done_gen
             FROM reminders WHERE done_gen IS NULL ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], row_to_reminder)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a reminder done at `generation`. Returns false when no such open
    /// reminder exists.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn reminder_done(&self, id: i64, generation: u64) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let changed = conn.execute(
            "UPDATE reminders SET done_gen = ?2 WHERE id = ?1 AND done_gen IS NULL",
            params![id, generation],
        )?;
        Ok(changed > 0)
    }

    // ── countdowns ─────────────────────────────────────────────────────

    /// Register a countdown; returns its id.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn countdown_add(
        &self,
        label: &str,
        start_date: &str,
        end_date: &str,
        total_work_days: u32,
        display: Option<&str>,
        generation: u64,
    ) -> Result<i64, StoreError> {
        let display = display.unwrap_or("{label}: work day {n} of {total}");
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute(
            "INSERT INTO countdowns (label, start_date, end_date, total_work_days, display, created_gen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![label, start_date, end_date, total_work_days, display, generation],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Active (non-retired) countdowns.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn countdowns_active(&self) -> Result<Vec<StoredCountdown>, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, label, start_date, end_date, total_work_days, display, created_gen, retired_gen
             FROM countdowns WHERE retired_gen IS NULL ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], row_to_countdown)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Retire a countdown at `generation`. Returns false when not found.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn countdown_retire(&self, id: i64, generation: u64) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let changed = conn.execute(
            "UPDATE countdowns SET retired_gen = ?2 WHERE id = ?1 AND retired_gen IS NULL",
            params![id, generation],
        )?;
        Ok(changed > 0)
    }

    // ── watches ────────────────────────────────────────────────────────

    /// Register a URL watch; returns its id.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn watch_add(&self, url: &str, note: &str, generation: u64) -> Result<i64, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute(
            "INSERT INTO watches (url, note, created_gen) VALUES (?1, ?2, ?3)",
            params![url, note, generation],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All watches.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn watches(&self) -> Result<Vec<Watch>, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, url, note, last_hash, last_seen_gen, created_gen FROM watches ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], row_to_watch)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove a watch. Returns false when not found.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn watch_remove(&self, id: i64) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        Ok(conn.execute("DELETE FROM watches WHERE id = ?1", params![id])? > 0)
    }

    /// Record a fetch outcome for a watch.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn watch_seen(&self, id: i64, hash: &str, generation: u64) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute(
            "UPDATE watches SET last_hash = ?2, last_seen_gen = ?3 WHERE id = ?1",
            params![id, hash, generation],
        )?;
        Ok(())
    }

    // ── export / import (sovereignty) ──────────────────────────────────

    /// Dump the whole store as a plain-JSON document.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn export_json(&self) -> Result<String, StoreError> {
        let dump = StoreDump {
            schema_version: SCHEMA_VERSION,
            last_generation: self.last_generation(),
            reminders: {
                let conn = self.conn.lock().expect("store lock poisoned");
                let mut stmt = conn.prepare(
                    "SELECT id, text, due, recurrence, created_gen, done_gen FROM reminders ORDER BY id",
                )?;
                let rows = stmt
                    .query_map([], row_to_reminder)?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            },
            countdowns: {
                let conn = self.conn.lock().expect("store lock poisoned");
                let mut stmt = conn.prepare(
                    "SELECT id, label, start_date, end_date, total_work_days, display, created_gen, retired_gen
                     FROM countdowns ORDER BY id",
                )?;
                let rows = stmt
                    .query_map([], row_to_countdown)?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            },
            watches: {
                let conn = self.conn.lock().expect("store lock poisoned");
                let mut stmt = conn.prepare(
                    "SELECT id, url, note, last_hash, last_seen_gen, created_gen FROM watches ORDER BY id",
                )?;
                let rows = stmt
                    .query_map([], row_to_watch)?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            },
        };
        Ok(serde_json::to_string_pretty(&dump).unwrap_or_else(|_| "{}".to_string()))
    }

    /// Import a [`Self::export_json`] dump (appends; ids are reassigned).
    ///
    /// # Errors
    /// [`StoreError::Import`] on malformed payloads, otherwise SQLite errors.
    pub fn import_json(&self, json: &str) -> Result<(), StoreError> {
        let dump: StoreDump =
            serde_json::from_str(json).map_err(|e| StoreError::Import(e.to_string()))?;
        for r in &dump.reminders {
            let id = self.reminder_add(
                &r.text,
                r.due.as_deref(),
                r.recurrence.as_deref(),
                r.created_gen,
            )?;
            if let Some(done) = r.done_gen {
                self.reminder_done(id, done)?;
            }
        }
        for c in &dump.countdowns {
            let id = self.countdown_add(
                &c.label,
                &c.start_date,
                &c.end_date,
                c.total_work_days,
                Some(&c.display),
                c.created_gen,
            )?;
            if let Some(retired) = c.retired_gen {
                self.countdown_retire(id, retired)?;
            }
        }
        for w in &dump.watches {
            let id = self.watch_add(&w.url, &w.note, w.created_gen)?;
            if let (Some(hash), Some(gen)) = (&w.last_hash, w.last_seen_gen) {
                self.watch_seen(id, hash, gen)?;
            }
        }
        Ok(())
    }
}

fn row_to_reminder(row: &rusqlite::Row<'_>) -> rusqlite::Result<Reminder> {
    Ok(Reminder {
        id: row.get(0)?,
        text: row.get(1)?,
        due: row.get(2)?,
        recurrence: row.get(3)?,
        created_gen: row.get(4)?,
        done_gen: row.get(5)?,
    })
}

fn row_to_countdown(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCountdown> {
    Ok(StoredCountdown {
        id: row.get(0)?,
        label: row.get(1)?,
        start_date: row.get(2)?,
        end_date: row.get(3)?,
        total_work_days: row.get(4)?,
        display: row.get(5)?,
        created_gen: row.get(6)?,
        retired_gen: row.get(7)?,
    })
}

fn row_to_watch(row: &rusqlite::Row<'_>) -> rusqlite::Result<Watch> {
    Ok(Watch {
        id: row.get(0)?,
        url: row.get(1)?,
        note: row.get(2)?,
        last_hash: row.get(3)?,
        last_seen_gen: row.get(4)?,
        created_gen: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminder_lifecycle() {
        let store = Store::in_memory().unwrap();
        let id = store
            .reminder_add("rotate the pagerduty token", Some("2026-06-10"), None, 3)
            .unwrap();
        let daily = store
            .reminder_add("check the board", None, Some("daily"), 3)
            .unwrap();

        let open = store.reminders_open().unwrap();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].text, "rotate the pagerduty token");
        assert_eq!(open[0].due.as_deref(), Some("2026-06-10"));
        assert_eq!(open[0].created_gen, 3);
        assert_eq!(open[1].recurrence.as_deref(), Some("daily"));

        assert!(store.reminder_done(id, 5).unwrap());
        assert!(!store.reminder_done(id, 6).unwrap(), "already done");
        assert_eq!(store.reminders_open().unwrap().len(), 1);
        let _ = daily;
    }

    #[test]
    fn countdown_lifecycle_and_default_display() {
        let store = Store::in_memory().unwrap();
        let id = store
            .countdown_add("PJ onboarding", "2026-06-01", "2026-07-15", 30, None, 1)
            .unwrap();
        let active = store.countdowns_active().unwrap();
        assert_eq!(active[0].display, "{label}: work day {n} of {total}");
        assert!(store.countdown_retire(id, 2).unwrap());
        assert!(store.countdowns_active().unwrap().is_empty());
    }

    #[test]
    fn watch_lifecycle_records_hash_and_generation() {
        let store = Store::in_memory().unwrap();
        let id = store
            .watch_add("https://example.com/releases", "new versions", 1)
            .unwrap();
        store.watch_seen(id, "abc123", 2).unwrap();
        let watches = store.watches().unwrap();
        assert_eq!(watches[0].last_hash.as_deref(), Some("abc123"));
        assert_eq!(watches[0].last_seen_gen, Some(2));
        assert!(store.watch_remove(id).unwrap());
        assert!(store.watches().unwrap().is_empty());
    }

    #[test]
    fn generation_persists() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.last_generation(), 0);
        store.set_last_generation(42).unwrap();
        assert_eq!(store.last_generation(), 42);
    }

    #[test]
    fn export_import_round_trips() {
        let a = Store::in_memory().unwrap();
        a.reminder_add("alpha", Some("2026-06-10"), None, 1)
            .unwrap();
        let done = a.reminder_add("beta", None, Some("weekly"), 1).unwrap();
        a.reminder_done(done, 2).unwrap();
        a.countdown_add(
            "ramp",
            "2026-06-01",
            "2026-07-01",
            20,
            Some("{label} {n}/{total}"),
            1,
        )
        .unwrap();
        let w = a.watch_add("https://example.com", "note", 1).unwrap();
        a.watch_seen(w, "h1", 2).unwrap();
        a.set_last_generation(2).unwrap();

        let json = a.export_json().unwrap();
        assert!(json.contains("alpha"), "plain-text export carries content");

        let b = Store::in_memory().unwrap();
        b.import_json(&json).unwrap();
        assert_eq!(b.reminders_open().unwrap().len(), 1); // beta is done
        assert_eq!(b.countdowns_active().unwrap().len(), 1);
        assert_eq!(b.watches().unwrap()[0].last_hash.as_deref(), Some("h1"));
    }

    #[test]
    fn open_creates_parent_dirs_and_reopens() {
        let dir = std::env::temp_dir().join(format!(
            "modulex-store-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let path = dir.join("nested").join("store.db");
        let store = Store::open(&path).unwrap();
        store.reminder_add("persisted", None, None, 1).unwrap();
        drop(store);

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.reminders_open().unwrap()[0].text, "persisted");
        std::fs::remove_dir_all(&dir).ok();
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn resolve_path_order() {
        // No env in tests (would race other tests): exercise config + default.
        let from_config = Store::resolve_path(Some("/x/store.db"), Some(Path::new("/home/u")));
        assert_eq!(from_config, PathBuf::from("/x/store.db"));
        let from_home = Store::resolve_path(None, Some(Path::new("/home/u")));
        assert_eq!(from_home, PathBuf::from("/home/u/.modulex/store.db"));
    }
}
