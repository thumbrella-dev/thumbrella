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
}

impl SqliteCacheBackend {
    /// Open (or create) a SQLite database at `path`, run migrations, and
    /// populate the maintenance table.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
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

    fn put_task(&self, key: String, value: String) -> DeferredFuture {
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let size = value.len() as i64;
                let conn = conn.lock().unwrap();
                conn.execute(
                    "INSERT INTO thumbrella(cache_key, value, size_bytes, last_accessed_at)
                          VALUES (?1, ?2, ?3, unixepoch())
                     ON CONFLICT(cache_key) DO UPDATE
                        SET value            = excluded.value,
                            size_bytes       = excluded.size_bytes,
                            last_accessed_at = unixepoch(),
                            access_count     = access_count + 1",
                    params![key, value, size],
                )
                .ok();
            })
            .await
            .ok();
        })
    }
}

// ── Schema migrations ─────────────────────────────────────────────────────────

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
            access_count     INTEGER NOT NULL DEFAULT 1
        );

        CREATE INDEX IF NOT EXISTS idx_thumbrella_lru
            ON thumbrella(last_accessed_at);

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
            'Delete the oldest entries until total stored data fits within ?1 GiB.',
            'DELETE FROM thumbrella WHERE cache_key IN (
                SELECT cache_key FROM thumbrella
                 ORDER BY last_accessed_at ASC
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
