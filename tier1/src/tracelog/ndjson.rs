//! Append-only NDJSON trace backend.
//!
//! Each [`ThumbTrace`] is serialised to a single JSON line and appended to the
//! configured file.  The file is opened in append mode and created if absent.
//!
//! ## Thread safety
//!
//! An internal `Mutex<BufWriter<File>>` serialises concurrent write tasks
//! within one process.  Each tier should use its own file to avoid
//! cross-process coordination (`trace-t1.ndjson`, `trace-t2.ndjson`, …).
//! Correlation across tiers is via `session_id` on [`ThumbTrace`].

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::{Arc, Mutex};

use crate::after::DeferredFuture;
use crate::result::ThumbTrace;
use crate::tracelog::TraceBackend;

// ── Backend ───────────────────────────────────────────────────────────────────

/// Append-only NDJSON file backend.  Thread-safe via an internal mutex.
pub struct NdjsonTraceBackend {
    writer: Arc<Mutex<BufWriter<File>>>,
    path:   String,
}

impl NdjsonTraceBackend {
    /// Open (or create) an NDJSON file at `path` for appending.
    pub fn open(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            writer: Arc::new(Mutex::new(BufWriter::new(file))),
            path:   path.to_string(),
        })
    }

    /// Path this backend is writing to.
    pub fn path(&self) -> &str { &self.path }

    /// Diagnostic check for a configured NDJSON trace path.
    ///
    /// Checks write access and free disk space without opening or creating
    /// the file.  Safe to call at any time.
    pub fn check(path: &str) -> crate::check::FileCheck {
        crate::check::check_file_path(path)
    }
}

impl TraceBackend for NdjsonTraceBackend {
    fn name(&self) -> &'static str { "ndjson" }

    fn record_task(&self, trace: Arc<ThumbTrace>) -> DeferredFuture {
        let writer = Arc::clone(&self.writer);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let Ok(mut line) = serde_json::to_string(&*trace) else { return };
                line.push('\n');
                if let Ok(mut guard) = writer.lock() {
                    let _ = guard.write_all(line.as_bytes());
                    let _ = guard.flush();
                }
            })
            .await
            .ok();
        })
    }
}
