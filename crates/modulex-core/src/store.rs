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

use agent_store::{Generation, SqliteBackend};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Environment variable overriding the store path (first in the search
/// order, before config's `[store] path` and the default).
pub const ENV_STORE: &str = "MODULEX_STORE";

/// Schema version stamped via `PRAGMA user_version`.
const SCHEMA_VERSION: i32 = 2;

/// The v1 schema: meta, reminders, countdowns, watches, plus the reserved
/// `ical_feeds` / `mcp_servers` tables. Extracted to a const so [`Store::migrate`]
/// can apply it version-guarded (a fresh DB runs v1 then v2; a v1 DB runs only v2).
const MIGRATION_V1: &str = "BEGIN;
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
     COMMIT;";

/// The v2 schema: the knowledge-board `cards` table and its `card_refs`
/// auxiliary (the `refs{label:url}` map and the `blocked_on[]` list). Dates are
/// display-only TEXT; coordination is the `*_gen` counters. `closed_gen` is
/// NULL while open and set when the lane is `done`/`dropped` — the card analogue
/// of `done_gen` / `retired_gen`.
const MIGRATION_V2: &str = "BEGIN;
     CREATE TABLE IF NOT EXISTS cards (
       rowid_id    INTEGER PRIMARY KEY,
       card_id     TEXT NOT NULL UNIQUE,
       project     TEXT NOT NULL DEFAULT '',
       lane        TEXT NOT NULL DEFAULT 'p2',
       context     TEXT NOT NULL DEFAULT '',
       summary     TEXT NOT NULL DEFAULT '',
       size        TEXT,
       status      TEXT,
       recurs      TEXT,
       expires     TEXT,
       created     TEXT,
       updated     TEXT,
       body        TEXT NOT NULL DEFAULT '',
       author      TEXT,
       source      TEXT,
       source_id   TEXT,
       created_gen INTEGER NOT NULL,
       updated_gen INTEGER NOT NULL,
       closed_gen  INTEGER
     );
     CREATE INDEX IF NOT EXISTS idx_cards_lane    ON cards(lane);
     CREATE INDEX IF NOT EXISTS idx_cards_project ON cards(project);
     CREATE INDEX IF NOT EXISTS idx_cards_status  ON cards(status);
     CREATE TABLE IF NOT EXISTS card_refs (
       card_rowid INTEGER NOT NULL REFERENCES cards(rowid_id) ON DELETE CASCADE,
       kind       TEXT NOT NULL,
       label      TEXT NOT NULL DEFAULT '',
       value      TEXT NOT NULL,
       ordinal    INTEGER NOT NULL DEFAULT 0
     );
     CREATE INDEX IF NOT EXISTS idx_card_refs_card ON card_refs(card_rowid);
     PRAGMA user_version = 2;
     COMMIT;";

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
    /// A failure surfaced by the agent-store substrate.
    #[error("store substrate: {0}")]
    Substrate(String),
}

fn substrate_err(e: agent_store::StoreError) -> StoreError {
    StoreError::Substrate(e.to_string())
}

/// One-time migration of the engine generation counter. Pre-substrate builds
/// stored it as a string in modulex's own `meta` table; it now lives in the
/// agent-store substrate. Idempotent — only runs while the substrate counter
/// is still 0, then drops the legacy row so there is one source of truth.
fn migrate_legacy_generation(
    conn: &Connection,
    db: &dyn agent_store::Backend,
) -> Result<(), StoreError> {
    let counter = Generation::new("last_generation");
    if counter.current(db).map_err(substrate_err)? != 0 {
        return Ok(());
    }
    let legacy: Option<u64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'last_generation'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|v| v.parse().ok());
    if let Some(generation) = legacy {
        counter.set(db, generation).map_err(substrate_err)?;
        conn.execute("DELETE FROM meta WHERE key = 'last_generation'", [])?;
    }
    Ok(())
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

/// A downstream MCP server registered behind modulex (issue #7, PR D).
///
/// The registry holds only the *invocation shape* — the program to spawn and
/// its static argv. Credentials are **never** stored here: the spawning step
/// resolves credential references at run time (the [`crate::credentials::Secret`]
/// model), so a server's secrets never touch this table, an export, or a
/// report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// Stable registry name (the primary key; how a step selects this server).
    pub name: String,
    /// The program to spawn as a stdio MCP server (e.g. `npx`, `uvx`, a path).
    /// This is what the exec leash checks before the process exists.
    pub command: String,
    /// Static arguments passed to `command`, in order.
    pub args: Vec<String>,
    /// Free-text note (what this server is for).
    pub note: String,
    /// Engine generation when registered (a counter, never wall-clock).
    pub created_gen: u64,
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

/// A reference entry on a card: either an entry of the `refs{label:url}` map
/// (`kind = "ref"`) or an item of the `blocked_on[]` list (`kind = "blocked_on"`,
/// ordered by `ordinal`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardRef {
    /// `"ref"` | `"blocked_on"`.
    pub kind: String,
    /// Ref label (the map key); empty for `blocked_on` entries.
    pub label: String,
    /// The URL or path.
    pub value: String,
    /// Position within an ordered list (`blocked_on`); 0 for refs.
    pub ordinal: i64,
}

/// A knowledge-board card. SQLite is the operational store; the markdown
/// frontmatter form (see `board_md`) is the portable export.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Card {
    /// Row id (the operational handle, like [`Reminder::id`]).
    pub id: i64,
    /// Stable frontmatter `id` (the markdown-sync key, unique per board).
    pub card_id: String,
    /// Owning project.
    pub project: String,
    /// Priority lane: `p0` | `p1` | `p2` | `done` | `dropped`.
    pub lane: String,
    /// Context bucket (`work`, `homelab`, …); empty = board root.
    pub context: String,
    /// One-line summary.
    pub summary: String,
    /// Story size (free-text, e.g. `3d`).
    pub size: Option<String>,
    /// Free-text status (e.g. `blocked`).
    pub status: Option<String>,
    /// Recurrence note (free-text, e.g. `1-2x weekly`).
    pub recurs: Option<String>,
    /// Sunset date (ISO, display only).
    pub expires: Option<String>,
    /// Creation date (ISO, display only).
    pub created: Option<String>,
    /// Last-updated date (ISO, display only).
    pub updated: Option<String>,
    /// Markdown body after the frontmatter.
    pub body: String,
    /// Scribe-ownership field (preserved on round-trip).
    pub author: Option<String>,
    /// Provenance source (preserved on round-trip).
    pub source: Option<String>,
    /// Provenance source id (preserved on round-trip).
    pub source_id: Option<String>,
    /// Engine generation when created.
    pub created_gen: u64,
    /// Engine generation of the last update.
    pub updated_gen: u64,
    /// Engine generation when closed (`None` = open; set when lane is
    /// `done`/`dropped`).
    pub closed_gen: Option<u64>,
    /// Refs and blocked-on entries, in stable order.
    #[serde(default)]
    pub refs: Vec<CardRef>,
}

/// Fields for creating or updating a card — no engine-managed ids/generations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CardInput {
    /// Stable frontmatter `id`.
    pub card_id: String,
    /// Owning project.
    pub project: String,
    /// Priority lane.
    pub lane: String,
    /// Context bucket; empty = board root.
    pub context: String,
    /// One-line summary.
    pub summary: String,
    /// Story size.
    pub size: Option<String>,
    /// Free-text status.
    pub status: Option<String>,
    /// Recurrence note.
    pub recurs: Option<String>,
    /// Sunset date.
    pub expires: Option<String>,
    /// Creation date.
    pub created: Option<String>,
    /// Last-updated date.
    pub updated: Option<String>,
    /// Markdown body.
    pub body: String,
    /// Scribe-ownership field.
    pub author: Option<String>,
    /// Provenance source.
    pub source: Option<String>,
    /// Provenance source id.
    pub source_id: Option<String>,
    /// Refs and blocked-on entries.
    #[serde(default)]
    pub refs: Vec<CardRef>,
}

/// Counts returned by a board-directory import.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    /// Cards inserted (new `card_id`).
    pub added: usize,
    /// Cards updated (existing `card_id`).
    pub updated: usize,
    /// Files skipped (parse errors, duplicate `card_id` within the walk).
    pub skipped: usize,
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
    /// All registered downstream MCP servers (invocation shapes only — no
    /// credentials). `#[serde(default)]` keeps dumps that predate the proxy
    /// substrate importable.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    /// All board cards (open and closed). `#[serde(default)]` keeps v1 dumps
    /// (which predate cards) importable.
    #[serde(default)]
    pub cards: Vec<Card>,
}

/// The store handle. Cheap to share behind an `Arc`; all access serialized
/// through one connection (SQLite is the bottleneck anyway, and routine
/// state traffic is tiny).
pub struct Store {
    backend: Mutex<SqliteBackend>,
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
            backend: Mutex::new(SqliteBackend::from_connection(conn)),
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
            backend: Mutex::new(SqliteBackend::from_connection(Connection::open_in_memory()?)),
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        // Version-guarded, idempotent steps: a fresh DB (v0) runs both; a v1 DB
        // runs only v2. `CREATE TABLE IF NOT EXISTS` keeps re-runs harmless.
        if version < 1 {
            conn.execute_batch(MIGRATION_V1)?;
        }
        if version < 2 {
            conn.execute_batch(MIGRATION_V2)?;
        }
        // The engine generation counter now lives in the agent-store substrate.
        Generation::ensure_schema(&*backend).map_err(substrate_err)?;
        migrate_legacy_generation(conn, &*backend)?;
        Ok(())
    }

    // ── generation persistence ─────────────────────────────────────────

    /// The persisted engine generation (0 when never set) — lets the engine
    /// stay monotonic across restarts.
    #[must_use]
    pub fn last_generation(&self) -> u64 {
        let backend = self.backend.lock().expect("store lock poisoned");
        Generation::new("last_generation")
            .current(&*backend)
            .unwrap_or(0)
    }

    /// Persist the engine generation after a run.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn set_last_generation(&self, generation: u64) -> Result<(), StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        Generation::new("last_generation")
            .set(&*backend, generation)
            .map_err(substrate_err)
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
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
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        Ok(conn.execute("DELETE FROM watches WHERE id = ?1", params![id])? > 0)
    }

    /// Record a fetch outcome for a watch.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn watch_seen(&self, id: i64, hash: &str, generation: u64) -> Result<(), StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        conn.execute(
            "UPDATE watches SET last_hash = ?2, last_seen_gen = ?3 WHERE id = ?1",
            params![id, hash, generation],
        )?;
        Ok(())
    }

    // ── mcp servers (downstream MCPs behind modulex; issue #7 PR D) ─────

    /// Register (or replace, by `name`) a downstream MCP server. Stores ONLY
    /// the invocation shape — command + static argv. Credential references are
    /// resolved by the calling step at spawn time and never persisted here.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn mcp_register(
        &self,
        name: &str,
        command: &str,
        args: &[String],
        note: &str,
        generation: u64,
    ) -> Result<(), StoreError> {
        let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_string());
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        conn.execute(
            "INSERT INTO mcp_servers (name, command, args_json, note, created_gen)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
               command = excluded.command, args_json = excluded.args_json,
               note = excluded.note, created_gen = excluded.created_gen",
            params![name, command, args_json, note, generation],
        )?;
        Ok(())
    }

    /// All registered downstream MCP servers, by name.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn mcp_servers(&self) -> Result<Vec<McpServer>, StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let mut stmt = conn.prepare(
            "SELECT name, command, args_json, note, created_gen FROM mcp_servers ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], row_to_mcp_server)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One registered server by name, or `None`.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn mcp_server(&self, name: &str) -> Result<Option<McpServer>, StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let server = conn
            .query_row(
                "SELECT name, command, args_json, note, created_gen
                 FROM mcp_servers WHERE name = ?1",
                params![name],
                row_to_mcp_server,
            )
            .optional()?;
        Ok(server)
    }

    /// Remove a registered server by name. Returns false when not found.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn mcp_unregister(&self, name: &str) -> Result<bool, StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        Ok(conn.execute("DELETE FROM mcp_servers WHERE name = ?1", params![name])? > 0)
    }

    // ── cards (knowledge board) ────────────────────────────────────────

    /// Insert or update a card (upsert on `card_id`); returns its rowid.
    /// `closed_gen` is set iff the lane is `done`/`dropped`. Refs and
    /// blocked-on entries are rewritten transactionally.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn card_add(&self, input: &CardInput, generation: u64) -> Result<i64, StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let tx = conn.unchecked_transaction()?;
        let closed = lane_is_closed(&input.lane).then_some(generation);
        let existing: Option<i64> = tx
            .query_row(
                "SELECT rowid_id FROM cards WHERE card_id = ?1",
                params![input.card_id],
                |r| r.get(0),
            )
            .optional()?;
        let rowid = if let Some(rowid) = existing {
            tx.execute(
                "UPDATE cards SET project=?2, lane=?3, context=?4, summary=?5, size=?6,
                   status=?7, recurs=?8, expires=?9, created=?10, updated=?11, body=?12,
                   author=?13, source=?14, source_id=?15, updated_gen=?16, closed_gen=?17
                 WHERE rowid_id=?1",
                params![
                    rowid,
                    input.project,
                    input.lane,
                    input.context,
                    input.summary,
                    input.size,
                    input.status,
                    input.recurs,
                    input.expires,
                    input.created,
                    input.updated,
                    input.body,
                    input.author,
                    input.source,
                    input.source_id,
                    generation,
                    closed,
                ],
            )?;
            rowid
        } else {
            tx.execute(
                "INSERT INTO cards (card_id, project, lane, context, summary, size, status,
                   recurs, expires, created, updated, body, author, source, source_id,
                   created_gen, updated_gen, closed_gen)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                params![
                    input.card_id,
                    input.project,
                    input.lane,
                    input.context,
                    input.summary,
                    input.size,
                    input.status,
                    input.recurs,
                    input.expires,
                    input.created,
                    input.updated,
                    input.body,
                    input.author,
                    input.source,
                    input.source_id,
                    generation,
                    generation,
                    closed,
                ],
            )?;
            tx.last_insert_rowid()
        };
        write_refs(&tx, rowid, &input.refs)?;
        tx.commit()?;
        Ok(rowid)
    }

    /// Fetch one card (with its refs) by rowid.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn card_get(&self, id: i64) -> Result<Option<Card>, StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let card = conn
            .query_row(
                &format!("SELECT {CARD_COLS} FROM cards WHERE rowid_id = ?1"),
                params![id],
                row_to_card,
            )
            .optional()?;
        match card {
            Some(mut c) => {
                c.refs = load_refs(conn, c.id)?;
                Ok(Some(c))
            }
            None => Ok(None),
        }
    }

    /// Fetch one card (with its refs) by stable `card_id` (the markdown-sync key).
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn card_by_card_id(&self, card_id: &str) -> Result<Option<Card>, StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let card = conn
            .query_row(
                &format!("SELECT {CARD_COLS} FROM cards WHERE card_id = ?1"),
                params![card_id],
                row_to_card,
            )
            .optional()?;
        match card {
            Some(mut c) => {
                c.refs = load_refs(conn, c.id)?;
                Ok(Some(c))
            }
            None => Ok(None),
        }
    }

    /// Update a card's mutable fields and refs, stamping `updated_gen`.
    /// Returns false when no card with `id` exists.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn card_update(
        &self,
        id: i64,
        input: &CardInput,
        generation: u64,
    ) -> Result<bool, StoreError> {
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let tx = conn.unchecked_transaction()?;
        let closed = lane_is_closed(&input.lane).then_some(generation);
        let changed = tx.execute(
            "UPDATE cards SET project=?2, lane=?3, context=?4, summary=?5, size=?6,
               status=?7, recurs=?8, expires=?9, created=?10, updated=?11, body=?12,
               author=?13, source=?14, source_id=?15, updated_gen=?16, closed_gen=?17
             WHERE rowid_id=?1",
            params![
                id,
                input.project,
                input.lane,
                input.context,
                input.summary,
                input.size,
                input.status,
                input.recurs,
                input.expires,
                input.created,
                input.updated,
                input.body,
                input.author,
                input.source,
                input.source_id,
                generation,
                closed,
            ],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        write_refs(&tx, id, &input.refs)?;
        tx.commit()?;
        Ok(true)
    }

    /// Move a card to a new lane (and optionally context), stamping
    /// `updated_gen`. Moving into `done`/`dropped` sets `closed_gen`; moving
    /// out clears it. Returns false when no such card.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn card_move(
        &self,
        id: i64,
        lane: &str,
        context: Option<&str>,
        generation: u64,
    ) -> Result<bool, StoreError> {
        let closed = lane_is_closed(lane).then_some(generation);
        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let changed = match context {
            Some(ctx) => conn.execute(
                "UPDATE cards SET lane=?2, context=?3, updated_gen=?4, closed_gen=?5
                 WHERE rowid_id=?1",
                params![id, lane, ctx, generation, closed],
            )?,
            None => conn.execute(
                "UPDATE cards SET lane=?2, updated_gen=?3, closed_gen=?4 WHERE rowid_id=?1",
                params![id, lane, generation, closed],
            )?,
        };
        Ok(changed > 0)
    }

    /// Close a card by moving it to a closed lane (`done` by default,
    /// `dropped` allowed), setting `closed_gen`. Returns false when no such card.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn card_close(&self, id: i64, lane: &str, generation: u64) -> Result<bool, StoreError> {
        self.card_move(id, lane, None, generation)
    }

    /// Cards in a lane (optionally scoped to a context), oldest first, with refs.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn cards_in_lane(
        &self,
        lane: &str,
        context: Option<&str>,
    ) -> Result<Vec<Card>, StoreError> {
        self.cards_query_inner(None, None, Some(lane), context)
    }

    /// Query cards by any combination of project/status/lane (all `None` =
    /// every card), oldest first, with refs.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure.
    pub fn cards_query(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        lane: Option<&str>,
    ) -> Result<Vec<Card>, StoreError> {
        self.cards_query_inner(project, status, lane, None)
    }

    fn cards_query_inner(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        lane: Option<&str>,
        context: Option<&str>,
    ) -> Result<Vec<Card>, StoreError> {
        let mut sql = format!("SELECT {CARD_COLS} FROM cards");
        let mut conds: Vec<&str> = Vec::new();
        let mut args: Vec<String> = Vec::new();
        if let Some(p) = project {
            conds.push("project = ?");
            args.push(p.to_string());
        }
        if let Some(s) = status {
            conds.push("status = ?");
            args.push(s.to_string());
        }
        if let Some(l) = lane {
            conds.push("lane = ?");
            args.push(l.to_string());
        }
        if let Some(c) = context {
            conds.push("context = ?");
            args.push(c.to_string());
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql.push_str(" ORDER BY rowid_id");

        let backend = self.backend.lock().expect("store lock poisoned");
        let conn = backend.connection();
        let mut stmt = conn.prepare(&sql)?;
        let mut cards = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), row_to_card)?
            .collect::<Result<Vec<_>, _>>()?;
        for card in &mut cards {
            card.refs = load_refs(conn, card.id)?;
        }
        Ok(cards)
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
                let backend = self.backend.lock().expect("store lock poisoned");
                let conn = backend.connection();
                let mut stmt = conn.prepare(
                    "SELECT id, text, due, recurrence, created_gen, done_gen FROM reminders ORDER BY id",
                )?;
                let rows = stmt
                    .query_map([], row_to_reminder)?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            },
            countdowns: {
                let backend = self.backend.lock().expect("store lock poisoned");
                let conn = backend.connection();
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
                let backend = self.backend.lock().expect("store lock poisoned");
                let conn = backend.connection();
                let mut stmt = conn.prepare(
                    "SELECT id, url, note, last_hash, last_seen_gen, created_gen FROM watches ORDER BY id",
                )?;
                let rows = stmt
                    .query_map([], row_to_watch)?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            },
            mcp_servers: self.mcp_servers()?,
            cards: self.cards_query(None, None, None)?,
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
        for s in &dump.mcp_servers {
            self.mcp_register(&s.name, &s.command, &s.args, &s.note, s.created_gen)?;
        }
        for c in &dump.cards {
            self.card_add(&card_input_from(c), c.created_gen)?;
        }
        Ok(())
    }

    // ── board directory sync (markdown <-> cards) ──────────────────────

    /// Import a board directory tree into the cards table. Walks
    /// `<root>/[<context>/]<lane>/*.md` (lanes `p0|p1|p2|done|dropped`),
    /// parses each file, derives lane/context from the path, and upserts by
    /// `card_id`. Follows symlinks (the lane-view convention) but dedups by
    /// `card_id`, so a source file and its lane symlink import once.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failure (parse/read errors are counted as
    /// `skipped`, never fatal).
    pub fn import_dir(&self, root: &Path, generation: u64) -> Result<ImportReport, StoreError> {
        let mut report = ImportReport::default();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // (context, lane, dir)
        let mut lane_dirs: Vec<(String, String, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if BOARD_LANES.contains(&name.as_str()) {
                    lane_dirs.push((String::new(), name, path));
                } else if let Ok(sub) = std::fs::read_dir(&path) {
                    for s in sub.filter_map(Result::ok) {
                        let sp = s.path();
                        let sname = s.file_name().to_string_lossy().into_owned();
                        if sp.is_dir() && BOARD_LANES.contains(&sname.as_str()) {
                            lane_dirs.push((name.clone(), sname, sp));
                        }
                    }
                }
            }
        }
        lane_dirs.sort();

        for (context, lane, dir) in lane_dirs {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|es| {
                    es.filter_map(Result::ok)
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "md"))
                        .collect()
                })
                .unwrap_or_default();
            files.sort();
            for path in files {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    report.skipped += 1;
                    continue;
                };
                let mut input = match crate::board_md::card_from_markdown(&text) {
                    Ok(input) => input,
                    Err(_) => {
                        report.skipped += 1;
                        continue;
                    }
                };
                if !seen.insert(input.card_id.clone()) {
                    report.skipped += 1; // a source file and its symlink
                    continue;
                }
                input.lane.clone_from(&lane);
                input.context.clone_from(&context);
                let existed = self.card_by_card_id(&input.card_id)?.is_some();
                self.card_add(&input, generation)?;
                if existed {
                    report.updated += 1;
                } else {
                    report.added += 1;
                }
            }
        }
        Ok(report)
    }

    /// Export every card to `<root>/<context>/<lane>/<card_id>.md` as flat real
    /// files (not symlink lane-views). Non-destructive: writes/overwrites the
    /// files it owns, never deletes others. Returns the number written.
    ///
    /// # Errors
    /// [`StoreError::Io`] when a directory or file cannot be written.
    pub fn export_dir(&self, root: &Path) -> Result<usize, StoreError> {
        let cards = self.cards_query(None, None, None)?;
        for card in &cards {
            let dir = if card.context.is_empty() {
                root.join(&card.lane)
            } else {
                root.join(&card.context).join(&card.lane)
            };
            std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(dir.clone(), e))?;
            let file = dir.join(format!("{}.md", card.card_id));
            let markdown = crate::board_md::card_to_markdown(card);
            std::fs::write(&file, markdown).map_err(|e| StoreError::Io(file.clone(), e))?;
        }
        Ok(cards.len())
    }
}

/// The recognized board lanes, in priority order.
const BOARD_LANES: &[&str] = &["p0", "p1", "p2", "done", "dropped"];

/// Column list for `cards` SELECTs, matching [`row_to_card`]'s field order.
const CARD_COLS: &str = "rowid_id, card_id, project, lane, context, summary, size, status, \
     recurs, expires, created, updated, body, author, source, source_id, \
     created_gen, updated_gen, closed_gen";

/// Lanes that mark a card closed (carry a `closed_gen`).
fn lane_is_closed(lane: &str) -> bool {
    matches!(lane, "done" | "dropped")
}

/// Rebuild a [`CardInput`] from a [`Card`] (for import / re-insert).
fn card_input_from(c: &Card) -> CardInput {
    CardInput {
        card_id: c.card_id.clone(),
        project: c.project.clone(),
        lane: c.lane.clone(),
        context: c.context.clone(),
        summary: c.summary.clone(),
        size: c.size.clone(),
        status: c.status.clone(),
        recurs: c.recurs.clone(),
        expires: c.expires.clone(),
        created: c.created.clone(),
        updated: c.updated.clone(),
        body: c.body.clone(),
        author: c.author.clone(),
        source: c.source.clone(),
        source_id: c.source_id.clone(),
        refs: c.refs.clone(),
    }
}

/// Rewrite a card's `card_refs` rows (delete-then-insert) within a transaction.
fn write_refs(
    tx: &rusqlite::Transaction<'_>,
    card_rowid: i64,
    refs: &[CardRef],
) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM card_refs WHERE card_rowid = ?1",
        params![card_rowid],
    )?;
    for r in refs {
        tx.execute(
            "INSERT INTO card_refs (card_rowid, kind, label, value, ordinal)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![card_rowid, r.kind, r.label, r.value, r.ordinal],
        )?;
    }
    Ok(())
}

/// Load a card's refs (both `ref` and `blocked_on` kinds), in stable order:
/// `blocked_on` first by `ordinal`, then `ref` entries by label.
fn load_refs(conn: &Connection, card_rowid: i64) -> Result<Vec<CardRef>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT kind, label, value, ordinal FROM card_refs
         WHERE card_rowid = ?1 ORDER BY kind, ordinal, label",
    )?;
    let rows = stmt
        .query_map(params![card_rowid], |row| {
            Ok(CardRef {
                kind: row.get(0)?,
                label: row.get(1)?,
                value: row.get(2)?,
                ordinal: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
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

/// Map a `cards` row to a [`Card`]. Column order must match [`CARD_COLS`].
/// `refs` is left empty — callers fill it via [`load_refs`].
fn row_to_mcp_server(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpServer> {
    let args_json: String = row.get(2)?;
    Ok(McpServer {
        name: row.get(0)?,
        command: row.get(1)?,
        // A malformed args_json degrades to an empty argv rather than failing
        // the read — the registry stays usable.
        args: serde_json::from_str(&args_json).unwrap_or_default(),
        note: row.get(3)?,
        created_gen: row.get(4)?,
    })
}

fn row_to_card(row: &rusqlite::Row<'_>) -> rusqlite::Result<Card> {
    Ok(Card {
        id: row.get(0)?,
        card_id: row.get(1)?,
        project: row.get(2)?,
        lane: row.get(3)?,
        context: row.get(4)?,
        summary: row.get(5)?,
        size: row.get(6)?,
        status: row.get(7)?,
        recurs: row.get(8)?,
        expires: row.get(9)?,
        created: row.get(10)?,
        updated: row.get(11)?,
        body: row.get(12)?,
        author: row.get(13)?,
        source: row.get(14)?,
        source_id: row.get(15)?,
        created_gen: row.get(16)?,
        updated_gen: row.get(17)?,
        closed_gen: row.get(18)?,
        refs: Vec::new(),
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
    fn mcp_server_lifecycle_register_list_get_unregister() {
        let store = Store::in_memory().unwrap();
        store
            .mcp_register(
                "gh",
                "npx",
                &["-y".into(), "@modelcontextprotocol/server-github".into()],
                "enterprise github",
                4,
            )
            .unwrap();
        let servers = store.mcp_servers().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "gh");
        assert_eq!(servers[0].command, "npx");
        assert_eq!(
            servers[0].args,
            vec!["-y", "@modelcontextprotocol/server-github"]
        );
        assert_eq!(servers[0].created_gen, 4);

        let one = store.mcp_server("gh").unwrap().unwrap();
        assert_eq!(one.command, "npx");
        assert!(store.mcp_server("nope").unwrap().is_none());

        // Re-register by the same name replaces (upsert), never duplicates.
        store
            .mcp_register("gh", "uvx", &["mcp-github".into()], "swapped", 5)
            .unwrap();
        let servers = store.mcp_servers().unwrap();
        assert_eq!(servers.len(), 1, "same name upserts to one row");
        assert_eq!(servers[0].command, "uvx");
        assert_eq!(servers[0].created_gen, 5);

        assert!(store.mcp_unregister("gh").unwrap());
        assert!(!store.mcp_unregister("gh").unwrap(), "already gone");
        assert!(store.mcp_servers().unwrap().is_empty());
    }

    #[test]
    fn mcp_servers_export_import_round_trips() {
        let a = Store::in_memory().unwrap();
        a.mcp_register("gh", "npx", &["-y".into(), "server-github".into()], "gh", 1)
            .unwrap();
        a.mcp_register("jira", "uvx", &["mcp-jira".into()], "", 2)
            .unwrap();

        let json = a.export_json().unwrap();
        assert!(json.contains("server-github"), "export carries the argv");
        // The export carries invocation shape only — never a credential.
        assert!(!json.contains("token"), "no credential material in export");

        let b = Store::in_memory().unwrap();
        b.import_json(&json).unwrap();
        let servers = b.mcp_servers().unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "gh");
        assert_eq!(servers[0].args, vec!["-y", "server-github"]);
        assert_eq!(servers[1].name, "jira");
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

    // ── cards (knowledge board, schema v2) ─────────────────────────────

    #[test]
    fn card_lifecycle_add_move_update_close() {
        let store = Store::in_memory().unwrap();
        let input = CardInput {
            card_id: "homelab-2026-06-09-vpn".into(),
            project: "homelab".into(),
            lane: "p2".into(),
            summary: "renew vpn cert".into(),
            size: Some("1d".into()),
            ..Default::default()
        };
        let id = store.card_add(&input, 1).unwrap();

        let c = store.card_get(id).unwrap().unwrap();
        assert_eq!(c.lane, "p2");
        assert_eq!(c.created_gen, 1);
        assert!(c.closed_gen.is_none(), "p2 is an open lane");

        // promote to active
        assert!(store.card_move(id, "p0", None, 2).unwrap());
        assert_eq!(store.cards_in_lane("p0", None).unwrap().len(), 1);
        assert!(store.card_get(id).unwrap().unwrap().closed_gen.is_none());

        // close
        assert!(store.card_close(id, "done", 3).unwrap());
        let c = store.card_get(id).unwrap().unwrap();
        assert_eq!(c.lane, "done");
        assert_eq!(c.closed_gen, Some(3));
        assert_eq!(c.updated_gen, 3);

        // a full update can reopen it
        let mut upd = card_input_from(&c);
        upd.lane = "p1".into();
        upd.status = Some("reopened".into());
        assert!(store.card_update(id, &upd, 4).unwrap());
        let c = store.card_get(id).unwrap().unwrap();
        assert_eq!(c.lane, "p1");
        assert!(
            c.closed_gen.is_none(),
            "moving out of done clears closed_gen"
        );
        assert_eq!(c.status.as_deref(), Some("reopened"));

        assert!(!store.card_close(9999, "done", 5).unwrap(), "missing id");
    }

    #[test]
    fn card_refs_round_trip_preserves_order() {
        let store = Store::in_memory().unwrap();
        let refs = vec![
            CardRef {
                kind: "blocked_on".into(),
                label: String::new(),
                value: "https://x/issues/1".into(),
                ordinal: 0,
            },
            CardRef {
                kind: "blocked_on".into(),
                label: String::new(),
                value: "https://x/issues/2".into(),
                ordinal: 1,
            },
            CardRef {
                kind: "ref".into(),
                label: "issue".into(),
                value: "https://x/issues/9".into(),
                ordinal: 0,
            },
            CardRef {
                kind: "ref".into(),
                label: "pr".into(),
                value: "https://x/pull/3".into(),
                ordinal: 0,
            },
        ];
        let input = CardInput {
            card_id: "p-1".into(),
            lane: "p1".into(),
            summary: "s".into(),
            refs: refs.clone(),
            ..Default::default()
        };
        let id = store.card_add(&input, 1).unwrap();
        assert_eq!(store.card_get(id).unwrap().unwrap().refs, refs);

        // updating refs replaces them wholesale
        let mut upd = card_input_from(&store.card_get(id).unwrap().unwrap());
        upd.refs = vec![CardRef {
            kind: "ref".into(),
            label: "doc".into(),
            value: "docs/d.md".into(),
            ordinal: 0,
        }];
        store.card_update(id, &upd, 2).unwrap();
        assert_eq!(store.card_get(id).unwrap().unwrap().refs.len(), 1);
    }

    #[test]
    fn card_add_upserts_by_card_id() {
        let store = Store::in_memory().unwrap();
        let mut input = CardInput {
            card_id: "dup-1".into(),
            lane: "p2".into(),
            summary: "first".into(),
            ..Default::default()
        };
        let id1 = store.card_add(&input, 1).unwrap();
        input.summary = "second".into();
        input.lane = "p0".into();
        let id2 = store.card_add(&input, 2).unwrap();

        assert_eq!(id1, id2, "same card_id upserts to one row");
        let c = store.card_get(id1).unwrap().unwrap();
        assert_eq!(c.summary, "second");
        assert_eq!(c.lane, "p0");
        assert_eq!(c.created_gen, 1, "created_gen preserved on upsert");
        assert_eq!(c.updated_gen, 2);
        assert_eq!(store.cards_query(None, None, None).unwrap().len(), 1);
    }

    #[test]
    fn cards_query_filters_by_project_status_lane() {
        let store = Store::in_memory().unwrap();
        store
            .card_add(
                &CardInput {
                    card_id: "a".into(),
                    project: "homelab".into(),
                    lane: "p0".into(),
                    status: Some("blocked".into()),
                    summary: "x".into(),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        store
            .card_add(
                &CardInput {
                    card_id: "b".into(),
                    project: "gilabot".into(),
                    lane: "p0".into(),
                    summary: "y".into(),
                    ..Default::default()
                },
                1,
            )
            .unwrap();

        assert_eq!(
            store
                .cards_query(Some("homelab"), None, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .cards_query(None, Some("blocked"), None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.cards_query(None, None, Some("p0")).unwrap().len(), 2);
        assert_eq!(
            store
                .cards_query(Some("gilabot"), Some("blocked"), None)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn migration_v1_to_v2_upgrades_cleanly() {
        // A DB stamped at v1 (no cards table) must upgrade without losing data.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute(
            "INSERT INTO reminders (text, created_gen) VALUES ('legacy', 1)",
            [],
        )
        .unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);

        let store = Store {
            backend: Mutex::new(SqliteBackend::from_connection(conn)),
        };
        store.migrate().unwrap();

        let v: i32 = store
            .backend
            .lock()
            .unwrap()
            .connection()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
        assert_eq!(store.reminders_open().unwrap().len(), 1, "v1 data intact");

        // the cards table is now usable
        store
            .card_add(
                &CardInput {
                    card_id: "x-1".into(),
                    lane: "p1".into(),
                    summary: "hi".into(),
                    ..Default::default()
                },
                2,
            )
            .unwrap();
        assert_eq!(store.cards_in_lane("p1", None).unwrap().len(), 1);
    }

    #[test]
    fn legacy_generation_is_migrated_into_substrate() {
        // Regression: pre-substrate builds stored the engine generation as a
        // string in modulex's own `meta` table. On first open with agent-store,
        // that value must carry into the substrate counter and the legacy row
        // must be dropped, so there is a single source of truth.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('last_generation', '7')",
            [],
        )
        .unwrap();

        let store = Store {
            backend: Mutex::new(SqliteBackend::from_connection(conn)),
        };
        store.migrate().unwrap();

        assert_eq!(store.last_generation(), 7, "legacy generation carried over");

        let backend = store.backend.lock().unwrap();
        let leftover: Option<String> = backend
            .connection()
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_generation'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(leftover, None, "legacy row dropped after migration");
    }

    #[test]
    fn cards_export_import_round_trips() {
        let a = Store::in_memory().unwrap();
        a.card_add(
            &CardInput {
                card_id: "c-1".into(),
                project: "homelab".into(),
                lane: "p1".into(),
                summary: "alpha".into(),
                refs: vec![CardRef {
                    kind: "ref".into(),
                    label: "issue".into(),
                    value: "https://x/1".into(),
                    ordinal: 0,
                }],
                ..Default::default()
            },
            1,
        )
        .unwrap();
        a.card_add(
            &CardInput {
                card_id: "c-2".into(),
                lane: "done".into(),
                summary: "beta".into(),
                ..Default::default()
            },
            1,
        )
        .unwrap();

        let json = a.export_json().unwrap();
        assert!(json.contains("alpha"), "plain-text export carries cards");

        let b = Store::in_memory().unwrap();
        b.import_json(&json).unwrap();
        assert_eq!(b.cards_query(None, None, None).unwrap().len(), 2);
        assert_eq!(
            b.cards_in_lane("done", None).unwrap()[0].closed_gen,
            Some(1),
            "closed lane re-derives closed_gen on import"
        );
        assert_eq!(b.card_by_card_id("c-1").unwrap().unwrap().refs.len(), 1);
    }

    #[test]
    fn dir_sync_round_trips() {
        let root = std::env::temp_dir().join(format!(
            "modulex-board-dir-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        // Lay out a board: a context with a lane and a card.
        let lane = root.join("homelab").join("p0");
        std::fs::create_dir_all(&lane).unwrap();
        std::fs::write(
            lane.join("vpn.md"),
            "---\nid: homelab-2026-06-09-vpn\nproject: homelab\nsummary: renew cert\nrefs:\n  issue: https://example.com/1\n---\n\nbody\n",
        )
        .unwrap();

        let a = Store::in_memory().unwrap();
        let report = a.import_dir(&root, 1).unwrap();
        assert_eq!(report.added, 1);
        let cards = a.cards_in_lane("p0", Some("homelab")).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].card_id, "homelab-2026-06-09-vpn");
        assert_eq!(cards[0].refs.len(), 1);

        // Export to a second tree, re-import into a fresh store: equal card set.
        let out = root.join("export");
        a.export_dir(&out).unwrap();
        assert!(out
            .join("homelab")
            .join("p0")
            .join("homelab-2026-06-09-vpn.md")
            .is_file());

        let b = Store::in_memory().unwrap();
        b.import_dir(&out, 1).unwrap();
        let bcards = b.cards_query(None, None, None).unwrap();
        assert_eq!(bcards.len(), 1);
        assert_eq!(bcards[0].lane, "p0");
        assert_eq!(bcards[0].context, "homelab");
        assert_eq!(bcards[0].summary, "renew cert");

        std::fs::remove_dir_all(&root).ok();
    }
}
