//! SQLite cache backend.
//!
//! # Schema
//!
//! ```sql
//! thumbrella(
//!     cache_key        TEXT PRIMARY KEY,
//!     value            TEXT NOT NULL,          -- ThumbResult as JSON
//!     size_bytes       INTEGER NOT NULL,        -- byte length of value
//!     last_accessed_at INTEGER NOT NULL,        -- Unix epoch seconds
//!     access_count     INTEGER NOT NULL DEFAULT 1
//! )
//! ```
//!
//! # Maintenance
//!
//! The database ships a `readme` table with ready-to-run SQL snippets
//! for common housekeeping tasks.  Run them from any SQLite client:
//!
//! ```sql
//! -- See what's available:
//! SELECT name, description FROM readme;
//!
//! -- Delete entries not accessed in the last 90 days:
//! DELETE FROM thumbrella
//!  WHERE last_accessed_at < unixepoch() - (90 * 86400);
//!
//! -- Delete oldest entries until total stored data is under 1 GiB:
//! DELETE FROM thumbrella WHERE cache_key IN (
//!     SELECT cache_key FROM thumb_cache
//!      ORDER BY last_accessed_at ASC
//!       LIMIT max(0, (SELECT count(*) FROM thumbrella) -
//!                    (SELECT count(*) FROM thumbrella
//!                      WHERE (SELECT sum(size_bytes) FROM thumbrella) > 1073741824))
//! );
//! ```
//!
//! The same SQL is stored verbatim in `readme.sql_template` with `?1`
//! as the parameter placeholder — paste and substitute as needed.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use crate::after::DeferredFuture;
use crate::cache::CacheBackend;

// ── Backend ───────────────────────────────────────────────────────────────────

/// SQLite-backed cache.  Thread-safe via an internal `Mutex<Connection>`.
pub struct SqliteCacheBackend {
    conn: Arc<Mutex<Connection>>,
    /// Maximum total size in bytes before eviction kicks in.
    /// `None` means unbounded (manual maintenance only).
    max_bytes: Option<u64>,
}

impl SqliteCacheBackend {
    /// Open (or create) a SQLite database at `path`, run migrations, and
    /// populate the maintenance table.  No size limit; cache grows unbounded.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        Self::open_with_limit(path, None)
    }

    /// Open with an optional byte-size limit.  After each `put()` the backend
    /// checks total stored bytes; if over `max_bytes`, the oldest entries
    /// (by `last_accessed_at`) are deleted until the total fits.
    pub fn open_with_limit(path: &str, max_bytes: Option<u64>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)), max_bytes })
    }

    /// Diagnostic check for a configured SQLite cache path.
    ///
    /// Checks write access, free disk space, and — when the file already
    /// exists — schema compatibility.  Never opens the database in write mode
    /// or runs migrations; safe to call at any time.
    pub fn check(path: &str) -> crate::check::FileCheck {
        let mut fc = crate::check::check_file_path(path);
        fc.sqlite_validation = Some(check_schema(path));
        fc
    }
}

impl CacheBackend for SqliteCacheBackend {
    fn name(&self) -> &'static str { "sqlite" }

    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        let conn = Arc::clone(&self.conn);
        let key  = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let conn = conn.lock().unwrap();
                // RETURNING requires SQLite ≥ 3.35; bundled rusqlite 0.32 ships 3.46.
                conn.query_row(
                    "UPDATE thumbrella
                        SET last_accessed_at = unixepoch(),
                            access_count     = access_count + 1
                      WHERE cache_key = ?1
                  RETURNING value",
                    [&key],
                    |row| row.get::<_, String>(0),
                ).ok()
            })
            .await
            .ok()
            .flatten()
        })
    }

    fn put(&self, key: String, value: String, cost: u8, expires_at: u64) -> DeferredFuture {
        let conn = Arc::clone(&self.conn);
        let max_bytes = self.max_bytes;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let size = value.len() as i64;
                let expires = expires_at as i64;
                let conn = conn.lock().unwrap();
                conn.execute(
                    "INSERT INTO thumbrella(cache_key, value, size_bytes, last_accessed_at, render_cost, expires_at)
                          VALUES (?1, ?2, ?3, unixepoch(), ?4, ?5)
                     ON CONFLICT(cache_key) DO UPDATE
                        SET value            = excluded.value,
                            size_bytes       = excluded.size_bytes,
                            last_accessed_at = unixepoch(),
                            render_cost      = excluded.render_cost,
                            expires_at       = excluded.expires_at,
                            access_count     = access_count + 1",
                    params![key, value, size, cost as i64, expires],
                )
                .ok();

                // Purge expired entries.
                conn.execute(
                    "DELETE FROM thumbrella WHERE expires_at <= unixepoch()",
                    [],
                ).ok();

                // Evict oldest entries if over the byte limit.
                if let Some(limit) = max_bytes {
                    evict_oldest(&conn, limit);
                }
            })
            .await
            .ok();
        })
    }
}

// ── Eviction ──────────────────────────────────────────────────────────────────

/// Delete oldest entries until total stored bytes fits within `max_bytes`.
///
/// Eviction order: oldest `last_accessed_at` first, then cheapest
/// `render_cost`.  This preserves recently-used, expensive-to-render entries.
///
/// Called from [`SqliteCacheBackend::put`] while the connection lock is held.
fn evict_oldest(conn: &Connection, max_bytes: u64) {
    // Check current total.  If under limit, nothing to do.
    let total: i64 = conn
        .query_row("SELECT COALESCE(SUM(size_bytes), 0) FROM thumbrella", [], |r| r.get(0))
        .unwrap_or(0);
    if (total as u64) <= max_bytes {
        return;
    }

    // Delete oldest entries until the total fits.  Use a running sum to
    // delete just enough entries — delete oldest first, cheaper first.
    conn.execute_batch(&format!(
        "DELETE FROM thumbrella WHERE cache_key IN (
            SELECT cache_key FROM (
                SELECT cache_key,
                       SUM(size_bytes) OVER (
                           ORDER BY last_accessed_at ASC, render_cost ASC
                       ) AS running_total
                  FROM thumbrella
            )
             WHERE running_total <= {excess}
        )",
        excess = (total as i64).saturating_sub(max_bytes as i64),
    ))
    .ok();
}

// ── Schema migrations ─────────────────────────────────────────────────────────

/// Read-only schema validation — called from [`SqliteCacheBackend::check`].
///
/// Opens the file with `SQLITE_OPEN_READ_ONLY` so no writes or migrations
/// are performed.  Returns [`Validation::not_configured`] when the file does
/// not yet exist (nothing to validate; `open` will create it correctly).
fn check_schema(path: &str) -> crate::check::Validation {
    if !std::path::Path::new(path).exists() {
        return crate::check::Validation::not_configured();
    }

    let conn = match Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c)  => c,
        Err(e) => return crate::check::Validation::error(format!("cannot open: {e}")),
    };

    // Quick integrity check — catches truncated / non-SQLite files.
    let integrity: String = match conn.query_row(
        "PRAGMA integrity_check(1);", [], |r| r.get(0),
    ) {
        Ok(s)  => s,
        Err(e) => return crate::check::Validation::error(format!("integrity_check failed: {e}")),
    };
    if integrity != "ok" {
        return crate::check::Validation::error(format!("integrity_check: {integrity}"));
    }

    // Confirm the thumbrella table exists with all expected columns.
    let mut stmt = match conn.prepare("PRAGMA table_info(thumbrella);") {
        Ok(s)  => s,
        Err(e) => return crate::check::Validation::error(format!("PRAGMA table_info failed: {e}")),
    };
    let cols: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(1)) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(e)   => return crate::check::Validation::error(format!("reading columns failed: {e}")),
    };

    if cols.is_empty() {
        return crate::check::Validation::error(
            "table 'thumbrella' not found — may be a different database"
        );
    }

    let required = ["cache_key", "value", "size_bytes", "last_accessed_at", "access_count", "render_cost", "expires_at"];
    let missing: Vec<&str> = required.iter()
        .copied()
        .filter(|c| !cols.iter().any(|col| col == c))
        .collect();

    if !missing.is_empty() {
        return crate::check::Validation::error(format!(
            "schema mismatch — missing column(s): {}",
            missing.join(", ")
        ));
    }

    crate::check::Validation::ok()
}

fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    // WAL mode: concurrent readers don't block a writer.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    Ok(())
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS thumbrella (
            cache_key        TEXT    NOT NULL PRIMARY KEY,
            value            TEXT    NOT NULL,
            size_bytes       INTEGER NOT NULL DEFAULT 0,
            last_accessed_at INTEGER NOT NULL DEFAULT (unixepoch()),
            access_count     INTEGER NOT NULL DEFAULT 1,
            render_cost      INTEGER NOT NULL DEFAULT 0,
            expires_at       INTEGER NOT NULL DEFAULT (unixepoch() + 86400 * 365)
        );

        CREATE INDEX IF NOT EXISTS idx_thumbrella_lru
            ON thumbrella(last_accessed_at, render_cost);

        CREATE INDEX IF NOT EXISTS idx_thumbrella_expiry
            ON thumbrella(expires_at);

        -- Human-readable stats view.
        CREATE VIEW IF NOT EXISTS cache_stats AS
        SELECT
            count(*)                                      AS entry_count,
            round(sum(size_bytes) / 1048576.0, 2)        AS total_mb,
            datetime(min(last_accessed_at), 'unixepoch') AS oldest_access,
            datetime(max(last_accessed_at), 'unixepoch') AS newest_access
        FROM thumbrella;

        -- Self-documenting maintenance recipes.
        CREATE TABLE IF NOT EXISTS readme (
            name         TEXT NOT NULL PRIMARY KEY,
            description  TEXT NOT NULL,
            sql_template TEXT NOT NULL
        );
    ")?;

    // Upsert maintenance recipes so they stay current across schema bumps.
    conn.execute_batch("
        INSERT OR REPLACE INTO readme VALUES (
            'drop_entries_by_days',
            'Delete entries whose last_accessed_at is older than ?1 days.',
            'DELETE FROM thumbrella WHERE last_accessed_at < unixepoch() - (?1 * 86400);'
        );

        INSERT OR REPLACE INTO readme VALUES (
            'drop_entries_by_gb',
            'Delete cheapest+oldest entries until total stored data fits within ?1 GiB.',
            'DELETE FROM thumbrella WHERE cache_key IN (
                SELECT cache_key FROM thumbrella
                 ORDER BY last_accessed_at ASC, render_cost ASC
                 LIMIT max(0, (
                     SELECT count(*) FROM thumbrella
                 ) - (
                     SELECT count(*) FROM thumbrella
                      WHERE (SELECT sum(size_bytes) FROM thumbrella) > CAST(?1 * 1073741824 AS INTEGER)
                 ))
             );'
        );

        INSERT OR REPLACE INTO readme VALUES (
            'vacuum',
            'Reclaim disk space after large deletions.',
            'VACUUM;'
        );
    ")?;

    Ok(())
}
