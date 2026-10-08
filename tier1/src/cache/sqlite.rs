//! SQLite cache backend.
//!
//! # Schema
//!
//! ```sql
//! thumbrella(
//!     cache_key        TEXT PRIMARY KEY,
//!     value            TEXT NOT NULL,          -- ThumbMedia as JSON, URL omitted
//!     size_bytes       INTEGER NOT NULL,        -- byte length of value
//!     last_accessed_at INTEGER NOT NULL,        -- Unix epoch seconds
//!     access_count     INTEGER NOT NULL DEFAULT 1
//!     fresh_until      INTEGER NOT NULL,        -- freshness deadline
//!     cache_until      INTEGER NOT NULL,        -- cache lookup deadline
//!     pin_until        INTEGER NOT NULL DEFAULT 0,
//!     expires_at       INTEGER NOT NULL         -- max(cache_until, pin_until)
//! )
//! thumbrella_pins(pin_id TEXT PRIMARY KEY, cache_key TEXT REFERENCES thumbrella(cache_key) ON DELETE CASCADE)
//! thumbrella_metadata(name TEXT PRIMARY KEY, value BLOB NOT NULL)
//! ```
//!
//! # Format versioning
//!
//! One database file can be shared by several binary versions, so every stored
//! key carries [`crate::TBR_CACHE_VERSION`] (`format_scoped_key`).  A build can
//! therefore only ever read rows it wrote, and an incompatible version simply
//! starts cold instead of decoding the wrong payload.  The version lives inside
//! this backend because it is a property of the serialized value.
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
//! as the parameter placeholder - paste and substitute as needed.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use crate::after::DeferredFuture;
use crate::cache::{CacheBackend, CacheEntry, unix_now_secs};
use crate::cache::{PinFuture, pins::{PinClaim, PinSecret}};

//  Backend

/// SQLite-backed cache.  Thread-safe via an internal `Mutex<Connection>`.
pub struct SqliteCacheBackend {
    conn: Arc<Mutex<Connection>>,
    pin_secret: PinSecret,
    /// Maximum total size in bytes before eviction kicks in.
    /// `None` means unbounded (manual maintenance only).
    max_bytes: Option<u64>,
}

impl SqliteCacheBackend {
    /// Open (or create) a SQLite database at `path`, run migrations, and
    /// populate the maintenance table.  No size limit; cache grows unbounded.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::open_with_limit(path, None)
    }

    /// Open with an optional byte-size limit.  After each `put()` the backend
    /// checks total stored bytes; if over `max_bytes`, the oldest entries
    /// (by `last_accessed_at`) are deleted until the total fits.
    pub fn open_with_limit(path: &str, max_bytes: Option<u64>) -> Result<Self, String> {
        let mut conn = Connection::open(path).map_err(|e| format!("open database: {e}"))?;
        apply_pragmas(&conn).map_err(|e| format!("configure database: {e}"))?;
        migrate(&conn).map_err(|e| format!("migrate database: {e}"))?;
        let pin_secret = load_pin_secret(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            pin_secret,
            max_bytes,
        })
    }

    /// Diagnostic check for a configured SQLite cache path.
    ///
    /// Checks write access, free disk space, and - when the file already
    /// exists - schema compatibility.  Never opens the database in write mode
    /// or runs migrations; safe to call at any time.
    pub fn check(path: &str) -> crate::check::FileCheck {
        let mut fc = crate::check::check_file_path(path);
        fc.sqlite_validation = Some(check_schema(path));
        fc
    }

    fn lookup<'a>(&'a self, key: &'a str, pin: bool, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        let conn = Arc::clone(&self.conn);
        let key = format_scoped_key(key);
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || -> rusqlite::Result<Option<CacheEntry>> {
                let conn = conn.lock().expect("SQLite cache mutex poisoned");
                let now = unix_now_secs();
                let pin_deadline = if pin_ttl == 0 { 0 } else { now.saturating_add(pin_ttl) };
                let pin_deadline = i64::try_from(pin_deadline).unwrap_or(i64::MAX);
                use rusqlite::OptionalExtension;
                conn.query_row(
                    "UPDATE thumbrella
                        SET last_accessed_at = unixepoch(),
                            access_count = access_count + 1,
                            pin_until = max(pin_until, ?2),
                            expires_at = max(cache_until, pin_until, ?2)
                      WHERE cache_key = ?1
                        AND CASE WHEN ?3 THEN pin_until ELSE cache_until END > unixepoch()
                  RETURNING value, cache_until, pin_until, last_accessed_at, render_cost, fresh_until",
                    params![key, pin_deadline, pin],
                    |row| {
                        let json: String = row.get(0)?;
                        let media = serde_json::from_str(&json).map_err(|e|
                            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
                        let cost: i64 = row.get(4)?;
                        Ok(CacheEntry {
                            media,
                            fresh_until: read_deadline(row, 5)?,
                            cache_until: read_deadline(row, 1)?,
                            pin_until: read_deadline(row, 2)?,
                            last_accessed_at: read_deadline(row, 3)?,
                            render_cost: u8::try_from(cost).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(4, cost))?,
                        })
                    },
                ).optional()
            }).await;
            match result {
                Ok(Ok(entry)) => entry,
                Ok(Err(e)) => {
                    tracing::warn!("sqlite cache: lookup failed - {e}");
                    None
                }
                Err(e) => {
                    tracing::warn!("sqlite cache: lookup task failed - {e}");
                    None
                }
            }
        })
    }

    fn write_entry(&self, key: String, mut entry: CacheEntry, only_if_absent: bool, expected_cache: Option<String>) -> DeferredFuture {
        let conn = Arc::clone(&self.conn);
        let max_bytes = self.max_bytes;
        let key = format_scoped_key(&key);
        Box::pin(async move {
            entry.media.url.clear();
            let value = match serde_json::to_string(&entry.media) {
                Ok(value) => value,
                Err(e) => {
                    tracing::warn!("sqlite cache: serialization failed - {e}");
                    return;
                }
            };
            let cache_until = i64::try_from(entry.cache_until).unwrap_or(i64::MAX);
            let fresh_until = i64::try_from(entry.fresh_until).unwrap_or(i64::MAX);
            let pin_until = i64::try_from(entry.pin_until).unwrap_or(i64::MAX);
            let accessed_at = i64::try_from(entry.last_accessed_at).unwrap_or(i64::MAX);
            tokio::task::spawn_blocking(move || {
                let size = value.len() as i64;
                let conn = conn.lock().unwrap();
                if only_if_absent && entry.retain_until() <= unix_now_secs() {
                    return;
                }
                match conn.execute(
                    "INSERT INTO thumbrella(cache_key, value, size_bytes, last_accessed_at, render_cost, cache_until, pin_until, expires_at, fresh_until)
                          SELECT ?1, ?2, ?3, ?7, ?4, ?5, ?6, max(?5, ?6), ?10
                           WHERE ?9 IS NULL OR NOT EXISTS (
                               SELECT 1 FROM thumbrella WHERE cache_key = ?1)
                               OR EXISTS (SELECT 1 FROM thumbrella WHERE cache_key = ?1 AND json_extract(value, '$.cache') = ?9)
                     ON CONFLICT(cache_key) DO UPDATE
                        SET value            = excluded.value,
                            size_bytes       = excluded.size_bytes,
                            last_accessed_at = excluded.last_accessed_at,
                            render_cost      = excluded.render_cost,
                            cache_until      = excluded.cache_until,
                            fresh_until      = excluded.fresh_until,
                            pin_until        = max(thumbrella.pin_until, excluded.pin_until),
                            expires_at       = max(excluded.cache_until, thumbrella.pin_until, excluded.pin_until),
                            access_count     = access_count + 1
                      WHERE (NOT ?8 OR max(thumbrella.cache_until, thumbrella.pin_until) <= unixepoch())
                        AND (?9 IS NULL OR json_extract(thumbrella.value, '$.cache') = ?9)",
                    params![key, value, size, i64::from(entry.render_cost), cache_until, pin_until, accessed_at, only_if_absent, expected_cache, fresh_until],
                ) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("sqlite cache: write failed - {e}");
                        return;
                    }
                }

                if let Err(e) = conn.execute("DELETE FROM thumbrella WHERE expires_at <= unixepoch()", []) {
                    tracing::warn!("sqlite cache: expiration purge failed - {e}");
                }
                if let Some(limit) = max_bytes {
                    evict_oldest(&conn, limit);
                }
            })
            .await
            .unwrap_or_else(|e| tracing::warn!("sqlite cache: write task failed - {e}"));
        })
    }
}

fn read_deadline(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

/// Scope a caller-supplied key to this build's cache format.
///
/// One SQLite file can be shared by several binary versions, so the stored key
/// carries [`crate::TBR_CACHE_VERSION`] and a build can only ever read rows it
/// wrote.  Keeping the version here - rather than in the pipeline's key - means
/// nothing upstream has to know about it.
fn format_scoped_key(key: &str) -> String {
    format!("v{}:{key}", crate::TBR_CACHE_VERSION)
}

impl CacheBackend for SqliteCacheBackend {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn pin_candidate(&self, kind: crate::media::FileKind, key: &str, attempt: u32) -> Result<String, String> {
        Ok(self.pin_secret.candidate(kind, key, attempt))
    }

    fn claim_pin<'a>(&'a self, id: &'a str, key: &'a str, ttl: u64) -> PinFuture<'a, PinClaim> {
        let conn = self.conn.clone();
        let id = format_scoped_key(id);
        let key = format_scoped_key(key);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> rusqlite::Result<PinClaim> {
                use rusqlite::OptionalExtension;
                let mut conn = conn.lock().expect("SQLite cache mutex poisoned");
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute("DELETE FROM thumbrella_pins WHERE NOT EXISTS (
                    SELECT 1 FROM thumbrella WHERE thumbrella.cache_key = thumbrella_pins.cache_key
                    AND pin_until > unixepoch())", [])?;
                let owner: Option<String> = tx.query_row(
                    "SELECT cache_key FROM thumbrella_pins WHERE pin_id = ?1", [&id], |row| row.get(0),
                ).optional()?;
                if owner.is_some_and(|owner| owner != key) {
                    return Ok(PinClaim::Conflict);
                }
                let deadline = i64::try_from(unix_now_secs().saturating_add(ttl)).unwrap_or(i64::MAX);
                let updated = tx.execute(
                    "UPDATE thumbrella SET pin_until = max(pin_until, ?2),
                        expires_at = max(cache_until, pin_until, ?2)
                     WHERE cache_key = ?1 AND expires_at > unixepoch()",
                    params![key, deadline],
                )?;
                if updated == 0 {
                    return Ok(PinClaim::Missing);
                }
                tx.execute("INSERT INTO thumbrella_pins(pin_id, cache_key) VALUES (?1, ?2)
                    ON CONFLICT(pin_id) DO NOTHING", params![id, key])?;
                tx.commit()?;
                Ok(PinClaim::Claimed)
            }).await.map_err(|e| format!("sqlite pin claim task: {e}"))?
                .map_err(|e| format!("sqlite pin claim: {e}"))
        })
    }

    fn pin_thumbnail<'a>(&'a self, id: &'a str) -> PinFuture<'a, Option<Vec<u8>>> {
        let conn = self.conn.clone();
        let id = format_scoped_key(id);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, String> {
                use rusqlite::OptionalExtension;
                let conn = conn.lock().expect("SQLite cache mutex poisoned");
                let value: Option<String> = conn.query_row(
                    "SELECT value FROM thumbrella JOIN thumbrella_pins USING(cache_key)
                     WHERE pin_id = ?1 AND pin_until > unixepoch()", [&id], |row| row.get(0),
                ).optional().map_err(|e| format!("sqlite pin lookup: {e}"))?;
                value.map(|value| serde_json::from_str::<crate::ThumbMedia>(&value)
                    .map(|media| media.thumbnail).map_err(|e| format!("sqlite pin payload: {e}"))).transpose()
            }).await.map_err(|e| format!("sqlite pin lookup task: {e}"))?
        })
    }

    fn get_entry<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        self.lookup(key, false, pin_ttl)
    }

    fn get_pin_entry<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        self.lookup(key, true, 0)
    }

    fn promote(&self, key: String, entry: CacheEntry) -> DeferredFuture {
        self.write_entry(key, entry, true, None)
    }

    fn revalidate(&self, key: String, entry: CacheEntry, expected_cache: String) -> DeferredFuture {
        self.write_entry(key, entry, false, Some(expected_cache))
    }

    fn put(&self, key: String, media: crate::result::ThumbMedia, cost: u8, cache_until: u64, pin_ttl: u64) -> DeferredFuture {
        let conn = Arc::clone(&self.conn);
        let max_bytes = self.max_bytes;
        let pin_secret = self.pin_secret.clone();
        Box::pin(async move {
            let backend = Self { conn, max_bytes, pin_secret };
            backend.write_entry(key, CacheEntry::new(media, cost, cache_until, pin_ttl, unix_now_secs()), false, None).await;
        })
    }
}

//  Eviction

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
    // delete just enough entries - delete oldest first, cheaper first.
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
        excess = total.saturating_sub(max_bytes as i64),
    ))
    .ok();
}

//  Schema migrations

/// Read-only schema validation - called from [`SqliteCacheBackend::check`].
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
        Ok(c) => c,
        Err(e) => return crate::check::Validation::error(format!("cannot open: {e}")),
    };

    // Quick integrity check - catches truncated / non-SQLite files.
    let integrity: String = match conn.query_row("PRAGMA integrity_check(1);", [], |r| r.get(0)) {
        Ok(s) => s,
        Err(e) => return crate::check::Validation::error(format!("integrity_check failed: {e}")),
    };
    if integrity != "ok" {
        return crate::check::Validation::error(format!("integrity_check: {integrity}"));
    }

    // Confirm the thumbrella table exists with all expected columns.
    let mut stmt = match conn.prepare("PRAGMA table_info(thumbrella);") {
        Ok(s) => s,
        Err(e) => return crate::check::Validation::error(format!("PRAGMA table_info failed: {e}")),
    };
    let cols: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(1)) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(e) => return crate::check::Validation::error(format!("reading columns failed: {e}")),
    };

    if cols.is_empty() {
        return crate::check::Validation::error("table 'thumbrella' not found - may be a different database");
    }

    let required = [
        "cache_key",
        "value",
        "size_bytes",
        "last_accessed_at",
        "access_count",
        "render_cost",
        "expires_at",
        "cache_until",
        "pin_until",
        "fresh_until",
    ];
    let missing: Vec<&str> = required.iter().copied().filter(|c| !cols.iter().any(|col| col == c)).collect();

    if !missing.is_empty() {
        return crate::check::Validation::error(format!(
            "schema mismatch - missing column(s): {}",
            missing.join(", ")
        ));
    }

    let metadata: bool = match conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'thumbrella_metadata')",
        [], |row| row.get(0),
    ) {
        Ok(exists) => exists,
        Err(error) => return crate::check::Validation::error(format!("checking pin metadata: {error}")),
    };
    if !metadata {
        return crate::check::Validation::warn("pin secret will be initialized when the cache opens");
    }
    use rusqlite::OptionalExtension;
    match conn.query_row(
        "SELECT value FROM thumbrella_metadata WHERE name = 'pin_secret'", [], |row| row.get::<_, Vec<u8>>(0),
    ).optional() {
        Ok(Some(bytes)) => {
            if let Err(error) = PinSecret::from_bytes(bytes) {
                return crate::check::Validation::error(error);
            }
        }
        Ok(None) => return crate::check::Validation::warn("pin secret will be initialized when the cache opens"),
        Err(error) => return crate::check::Validation::error(format!("reading pin secret: {error}")),
    }

    crate::check::Validation::ok()
}

fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    // WAL mode: concurrent readers don't block a writer.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
    Ok(())
}

fn load_pin_secret(conn: &mut Connection) -> Result<PinSecret, String> {
    use rusqlite::OptionalExtension;

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("pin secret transaction: {e}"))?;
    let bytes: Option<Vec<u8>> = tx.query_row(
        "SELECT value FROM thumbrella_metadata WHERE name = 'pin_secret'", [], |row| row.get(0),
    ).optional().map_err(|e| format!("read pin secret: {e}"))?;
    let secret = match bytes {
        Some(bytes) => PinSecret::from_bytes(bytes)?,
        None => {
            let secret = PinSecret::generate().map_err(|e| format!("cannot generate pin secret: {e}"))?;
            tx.execute("INSERT INTO thumbrella_metadata(name, value) VALUES ('pin_secret', ?1)", [secret.as_bytes()])
                .map_err(|e| format!("store pin secret: {e}"))?;
            secret
        }
    };
    tx.commit().map_err(|e| format!("commit pin secret: {e}"))?;
    Ok(secret)
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
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

        CREATE TABLE IF NOT EXISTS thumbrella_pins (
            pin_id TEXT NOT NULL PRIMARY KEY,
            cache_key TEXT NOT NULL REFERENCES thumbrella(cache_key) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_thumbrella_pins_key ON thumbrella_pins(cache_key);

        CREATE TABLE IF NOT EXISTS thumbrella_metadata (
            name TEXT NOT NULL PRIMARY KEY,
            value BLOB NOT NULL
        );

        -- Human-readable stats view.
        CREATE VIEW IF NOT EXISTS cache_stats AS
        SELECT
            count(*)                                     AS entry_count,
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
    ",
    )?;

    let transaction = conn.unchecked_transaction()?;
    let mut stmt = transaction.prepare("PRAGMA table_info(thumbrella)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "cache_until") {
        transaction.execute_batch(
            "ALTER TABLE thumbrella ADD COLUMN cache_until INTEGER NOT NULL DEFAULT 0;
             UPDATE thumbrella SET cache_until = expires_at;"
        )?;
    }
    if !columns.iter().any(|column| column == "pin_until") {
        transaction.execute_batch("ALTER TABLE thumbrella ADD COLUMN pin_until INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !columns.iter().any(|column| column == "fresh_until") {
        transaction.execute_batch("ALTER TABLE thumbrella ADD COLUMN fresh_until INTEGER NOT NULL DEFAULT 0;")?;
        let mut entries = transaction.prepare("SELECT cache_key, value FROM thumbrella")?;
        let rows = entries.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        for row in rows {
            let (key, json) = row?;
            let media = match serde_json::from_str::<crate::result::ThumbMedia>(&json) {
                Ok(media) => media,
                Err(error) => {
                    tracing::warn!("sqlite cache: freshness migration for {key} failed - {error}");
                    continue;
                }
            };
            let fresh_until = i64::try_from(CacheEntry::freshness_deadline(&media.cache)).unwrap_or(i64::MAX);
            transaction.execute("UPDATE thumbrella SET fresh_until = ?2 WHERE cache_key = ?1", params![key, fresh_until])?;
        }
    }
    drop(stmt);
    transaction.commit()?;

    // Upsert maintenance recipes so they stay current across schema bumps.
    conn.execute_batch(
        "
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
    ",
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ThumbMedia;
    use crate::source::CacheHints;

    fn temporary_database(label: &str) -> std::path::PathBuf {
        let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("thumbrella-{label}-{}-{suffix}.db", std::process::id()))
    }

    #[tokio::test]
    async fn pin_indices_persist_across_reopen_and_are_removed_with_their_entry() {
        let path = temporary_database("pin");
        let filename = path.to_str().unwrap();
        let id;
        {
            let backend = SqliteCacheBackend::open(filename).unwrap();
            backend.put("source".into(), ThumbMedia { thumbnail: vec![1, 2], kind: crate::FileKind::Image, ..Default::default() },
                0, unix_now_secs() + 60, 60).await;
            id = backend.issue_pin(crate::FileKind::Image, "source", 60).await.unwrap().unwrap();
        }
        {
            let backend = SqliteCacheBackend::open(filename).unwrap();
            assert_eq!(id, backend.pin_candidate(crate::FileKind::Image, "source", 0).unwrap());
            assert_eq!(backend.pin_thumbnail(&id).await.unwrap(), Some(vec![1, 2]));
            let conn = backend.conn.lock().unwrap();
            conn.execute("DELETE FROM thumbrella", []).unwrap();
            let count: i64 = conn.query_row("SELECT count(*) FROM thumbrella_pins", [], |row| row.get(0)).unwrap();
            assert_eq!(count, 0);
            let length: i64 = conn.query_row(
                "SELECT length(value) FROM thumbrella_metadata WHERE name = 'pin_secret'", [], |row| row.get(0),
            ).unwrap();
            assert_eq!(length, 32);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_openers_initialize_one_persisted_pin_secret() {
        let path = temporary_database("pin-secret");
        {
            let conn = Connection::open(&path).unwrap();
            apply_pragmas(&conn).unwrap();
            migrate(&conn).unwrap();
        }
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8).map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let backend = SqliteCacheBackend::open(path.to_str().unwrap()).unwrap();
                backend.pin_candidate(crate::FileKind::Image, "source", 0).unwrap()
            })
        }).collect();
        let ids: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert!(ids.iter().all(|id| id == &ids[0]));
        let other = SqliteCacheBackend::open(":memory:").unwrap();
        assert_ne!(ids[0], other.pin_candidate(crate::FileKind::Image, "source", 0).unwrap());
        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM thumbrella_metadata WHERE name = 'pin_secret' AND length(value) = 32",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 1);
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_persisted_pin_secrets_fail_setup_and_diagnostics_without_replacing_them() {
        let path = temporary_database("invalid-pin-secret");
        {
            let conn = Connection::open(&path).unwrap();
            migrate(&conn).unwrap();
            conn.execute("INSERT INTO thumbrella_metadata VALUES ('pin_secret', ?1)", [vec![1u8; 31]]).unwrap();
        }
        let error = match SqliteCacheBackend::open(path.to_str().unwrap()) {
            Ok(_) => panic!("invalid secret was accepted"),
            Err(error) => error,
        };
        assert!(error.contains("exactly 32 bytes"), "{error}");
        assert_eq!(check_schema(path.to_str().unwrap()).status, crate::check::ValidationStatus::Error);
        let conn = Connection::open(&path).unwrap();
        let bytes: Vec<u8> = conn.query_row(
            "SELECT value FROM thumbrella_metadata WHERE name = 'pin_secret'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(bytes, vec![1u8; 31]);
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn eviction_does_not_remove_or_rotate_the_pin_secret() {
        let backend = SqliteCacheBackend::open_with_limit(":memory:", Some(1)).unwrap();
        let id = backend.pin_candidate(crate::FileKind::Image, "source", 0).unwrap();
        backend.put("source".into(), ThumbMedia { thumbnail: vec![1, 2], ..Default::default() },
            0, unix_now_secs() + 60, 60).await;
        backend.put("other".into(), ThumbMedia { thumbnail: vec![1, 2], ..Default::default() },
            1, unix_now_secs() + 60, 60).await;
        assert!(backend.get_entry("source", 0).await.is_none());
        assert_eq!(id, backend.pin_candidate(crate::FileKind::Image, "source", 0).unwrap());
        let conn = backend.conn.lock().unwrap();
        let length: i64 = conn.query_row(
            "SELECT length(value) FROM thumbrella_metadata WHERE name = 'pin_secret'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(length, 32);
    }

    #[test]
    fn migration_preserves_existing_cache_deadlines() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE thumbrella (
                cache_key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                last_accessed_at INTEGER NOT NULL DEFAULT 0,
                access_count INTEGER NOT NULL DEFAULT 1,
                render_cost INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER NOT NULL
             );"
        ).unwrap();
        let media = ThumbMedia {
            cache: CacheHints { expires_at: Some(100), ..Default::default() }.encode(60),
            ..Default::default()
        };
        conn.execute(
            "INSERT INTO thumbrella(cache_key, value, expires_at) VALUES ('old', ?1, 12345)",
            params![serde_json::to_string(&media).unwrap()],
        ).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let deadlines: (i64, i64, i64, i64) = conn.query_row(
            "SELECT cache_until, pin_until, expires_at, fresh_until FROM thumbrella WHERE cache_key = 'old'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap();
        assert_eq!(deadlines, (12345, 0, 12345, 100));
    }

    #[tokio::test]
    async fn freshness_migration_preserves_existing_dual_lifetimes_and_cache_policy() {
        let backend = SqliteCacheBackend::open(":memory:").unwrap();
        let now = unix_now_secs();
        for (key, no_cache) in [("fresh", false), ("revalidate", true)] {
            backend.put(key.into(), ThumbMedia {
                cache: CacheHints {
                    expires_at: Some(now + 100), no_cache, ..Default::default()
                }.encode(60),
                ..Default::default()
            }, 73, now + 300, 500).await;
        }
        let conn = backend.conn.lock().unwrap();
        conn.execute_batch("ALTER TABLE thumbrella DROP COLUMN fresh_until").unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        for (key, expected_fresh) in [("fresh", now + 100), ("revalidate", 0)] {
            let deadlines: (i64, i64, i64, i64) = conn.query_row(
                "SELECT fresh_until, cache_until, pin_until, render_cost FROM thumbrella WHERE cache_key = ?1",
                params![format_scoped_key(key)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).unwrap();
            assert_eq!(deadlines.0, i64::try_from(expected_fresh).unwrap());
            assert_eq!(deadlines.1, i64::try_from(now + 300).unwrap());
            assert!(deadlines.2 >= i64::try_from(now + 500).unwrap());
            assert_eq!(deadlines.3, 73);
        }
    }

    #[tokio::test]
    async fn one_row_tracks_both_lifetimes_and_never_stores_source_url() {
        let backend = SqliteCacheBackend::open(":memory:").unwrap();
        let now = unix_now_secs();
        let media = ThumbMedia {
            url: "https://example.com/private-source".into(),
            cache: CacheHints::expiring_in(600).encode(600),
            thumbnail: vec![1, 2, 3],
            ..Default::default()
        };
        let expected_fresh = CacheEntry::freshness_deadline(&media.cache);
        backend.put("entry".into(), media, 3, now + 600, 1200).await;
        assert!(backend.get("entry", 2400).await.is_some());
        assert!(backend.get("entry", 1).await.is_some());
        assert!(backend.get_pin("entry").await.is_some());
        let conn = backend.conn.lock().unwrap();
        let (count, value, cache_until, pin_until, expires_at, access_count, fresh_until): (i64, String, i64, i64, i64, i64, i64) = conn.query_row(
            "SELECT count(*), value, cache_until, pin_until, expires_at, access_count, fresh_until FROM thumbrella",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        ).unwrap();
        assert_eq!(count, 1);
        assert!(!value.contains("private-source"));
        assert_eq!(cache_until, i64::try_from(now + 600).unwrap());
        assert_eq!(fresh_until, i64::try_from(expected_fresh).unwrap());
        assert!(pin_until >= i64::try_from(now + 2400).unwrap());
        assert_eq!(expires_at, pin_until);
        assert_eq!(access_count, 4);
        conn.execute("UPDATE thumbrella SET pin_until = unixepoch() - 1", []).unwrap();
        drop(conn);
        assert!(backend.get_pin("entry").await.is_none());
        assert!(backend.get("entry", 0).await.is_some());
    }
}
