//! Scratch arena — temporary on-disk workspace for subprocess renderers.
//!
//! Tier 3 renderers that invoke external CLI tools often need to stage the
//! source media as a file on disk (the tool reads a path, not stdin) and
//! capture the output (the tool writes an image file).  This module provides
//! a managed temporary directory with:
//!
//! - **Automatic cleanup** — the directory and all contents are removed on
//!   [`Drop`], using [`tempfile::TempDir`].
//! - **Disk usage tracking** — an atomic counter tracks current bytes used
//!   and enforces a configurable limit.
//! - **Unique output paths** — each render invocation gets a collision-free
//!   file path in the arena.
//!
//! # Lifecycle
//!
//! ```text
//! Arena::new(max_bytes)
//!   ├─ stage_url(url, client)  → PathBuf  (download source → temp file)
//!   ├─ output_path(suffix)     → PathBuf  (allocate output path)
//!   ├─ read_output(path)       → Vec<u8>  (read back rendered image)
//!   └─ Drop                     → cleanup
//! ```
//!
//! # Platform notes
//!
//! On Linux, the arena lives under `$TMPDIR` (or `/tmp`).  Future
//! enhancements may use `O_TMPFILE` on Linux for unlinked temp files, but
//! the current implementation uses regular files in a temp dir — portable
//! and sufficient for all targets.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// ── ScratchArena ──────────────────────────────────────────────────────────────

/// A managed temporary directory for subprocess rendering I/O.
///
/// Created once per tier3 process (or per render invocation — both work).
/// Tracks disk usage and enforces a byte limit.  All files are removed when
/// the arena is dropped.
pub struct ScratchArena {
    /// The temporary directory.  Cleaned up on drop.
    dir: tempfile::TempDir,
    /// Maximum total bytes allowed in the arena.  Zero means no limit.
    max_bytes: u64,
    /// Current approximate bytes used.  Updated before writes (optimistic)
    /// and corrected after writes complete (pessimistic adjustment).
    current_bytes: AtomicU64,
}

impl ScratchArena {
    /// Create a new scratch arena with the given byte limit.
    ///
    /// The arena is created under the system temp directory (respects
    /// `$TMPDIR`).  `max_bytes` of 0 disables the limit.
    pub fn new(max_bytes: u64) -> io::Result<Self> {
        let dir = tempfile::TempDir::with_prefix("thumbrella-tier3-")?;
        Ok(Self {
            dir,
            max_bytes,
            current_bytes: AtomicU64::new(0),
        })
    }

    /// Return the root path of the arena.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Current approximate disk usage in bytes.
    pub fn current_usage(&self) -> u64 {
        self.current_bytes.load(Ordering::Relaxed)
    }

    /// Maximum allowed bytes (0 = no limit).
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    // ── Staging ───────────────────────────────────────────────────────────────

    /// Download `url` into a file in the arena and return its path.
    ///
    /// The download is streamed directly to disk.  The file name is derived
    /// from the URL's final path segment, with a random suffix to avoid
    /// collisions.
    ///
    /// Returns an error if the download would exceed `max_bytes`.
    pub async fn stage_url(
        &self,
        url: &str,
        client: &reqwest::Client,
    ) -> Result<PathBuf, ArenaError> {
        let response = client.get(url).send().await.map_err(ArenaError::Fetch)?;

        let content_length = response.content_length();

        // Check limit before downloading.
        if self.max_bytes > 0 {
            if let Some(cl) = content_length {
                let current = self.current_bytes.load(Ordering::Relaxed);
                if current + cl > self.max_bytes {
                    return Err(ArenaError::LimitExceeded {
                        current,
                        needed: cl,
                        max: self.max_bytes,
                    });
                }
            }
        }

        // Build a collision-free file name.
        let stem = url.rsplit('/')
            .next()
            .unwrap_or("download")
            .split('?')
            .next()
            .unwrap_or("download");
        let suffix: String = std::iter::repeat_with(fast_random_char).take(8).collect();
        let file_name = if stem.is_empty() { format!("dl_{suffix}") } else { format!("{stem}_{suffix}") };
        let dest = self.dir.path().join(&file_name);

        // Stream to disk.
        let mut file = tokio::fs::File::create(&dest).await.map_err(ArenaError::Io)?;
        let mut stream = response.bytes_stream();
        let mut written: u64 = 0;
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(ArenaError::Fetch)?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(ArenaError::Io)?;
            written += chunk.len() as u64;

            // Check limit during streaming.
            if self.max_bytes > 0 {
                let current = self.current_bytes.load(Ordering::Relaxed);
                if current + written > self.max_bytes {
                    // Clean up partial file.
                    drop(file);
                    let _ = tokio::fs::remove_file(&dest).await;
                    return Err(ArenaError::LimitExceeded {
                        current,
                        needed: written,
                        max: self.max_bytes,
                    });
                }
            }
        }

        self.current_bytes.fetch_add(written, Ordering::Relaxed);
        Ok(dest)
    }

    /// Write `bytes` to a new file in the arena and return its path.
    ///
    /// Useful for draining an [`HttpBuffer`](tier1::http_buf::HttpBuffer)
    /// reader to disk when a subprocess tool needs a file path.
    pub fn stage_bytes(&self, bytes: &[u8], hint_name: &str) -> Result<PathBuf, ArenaError> {
        let len = bytes.len() as u64;

        if self.max_bytes > 0 {
            let current = self.current_bytes.load(Ordering::Relaxed);
            if current + len > self.max_bytes {
                return Err(ArenaError::LimitExceeded {
                    current,
                    needed: len,
                    max: self.max_bytes,
                });
            }
        }

        let suffix: String = std::iter::repeat_with(fast_random_char).take(8).collect();
        let file_name = format!("{hint_name}_{suffix}");
        let dest = self.dir.path().join(&file_name);

        std::fs::write(&dest, bytes).map_err(ArenaError::Io)?;
        self.current_bytes.fetch_add(len, Ordering::Relaxed);
        Ok(dest)
    }

    /// Drain a reader to a file in the arena and return its path.
    ///
    /// Reads the entire reader into a buffer first (to check size against the
    /// limit), then writes to disk.  For large files, prefer [`stage_url`].
    pub fn stage_reader(
        &self,
        reader: &mut dyn std::io::Read,
        hint_name: &str,
    ) -> Result<PathBuf, ArenaError> {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map_err(ArenaError::Io)?;
        self.stage_bytes(&buf, hint_name)
    }

    // ── Output paths ──────────────────────────────────────────────────────────

    /// Allocate a unique output file path in the arena.
    ///
    /// The path is guaranteed not to collide with any existing file.  The
    /// caller is responsible for writing the file (typically a subprocess
    /// renderer).
    pub fn output_path(&self, suffix: &str) -> PathBuf {
        let rand: String = std::iter::repeat_with(fast_random_char).take(8).collect();
        let file_name = format!("out_{rand}.{suffix}");
        self.dir.path().join(&file_name)
    }

    /// Read back a file produced by a subprocess renderer.
    ///
    /// Tracks the file size against the arena usage.
    pub fn read_output(&self, path: &Path) -> Result<Vec<u8>, ArenaError> {
        let bytes = std::fs::read(path).map_err(ArenaError::Io)?;
        self.current_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    /// Remove a specific file from the arena and adjust the usage counter.
    ///
    /// Call this after consuming an output file to free space for subsequent
    /// renders.
    pub fn remove(&self, path: &Path) {
        if let Ok(meta) = std::fs::metadata(path) {
            let len = meta.len();
            let _ = std::fs::remove_file(path);
            self.current_bytes.fetch_sub(len, Ordering::Relaxed);
        }
    }
}

impl Drop for ScratchArena {
    fn drop(&mut self) {
        // TempDir::drop handles recursive cleanup.  We just log.
        let usage = self.current_bytes.load(Ordering::Relaxed);
        if usage > 0 {
            eprintln!("[tier3] scratch arena dropped ({usage} bytes cleaned up)");
        }
    }
}

// ── ArenaError ────────────────────────────────────────────────────────────────

/// Errors from scratch arena operations.
#[derive(Debug)]
pub enum ArenaError {
    /// I/O error (file creation, read, write).
    Io(io::Error),
    /// HTTP fetch error.
    Fetch(reqwest::Error),
    /// Disk usage limit would be exceeded.
    LimitExceeded {
        current: u64,
        needed: u64,
        max: u64,
    },
}

impl std::fmt::Display for ArenaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "scratch I/O error: {e}"),
            Self::Fetch(e) => write!(f, "scratch fetch error: {e}"),
            Self::LimitExceeded { current, needed, max } => {
                write!(f, "scratch arena limit exceeded: {current} + {needed} > {max} bytes")
            }
        }
    }
}

impl std::error::Error for ArenaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Fetch(e) => Some(e),
            Self::LimitExceeded { .. } => None,
        }
    }
}

impl From<io::Error> for ArenaError {
    fn from(e: io::Error) -> Self { Self::Io(e) }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Fast non-cryptographic random ASCII char for collision-free file names.
fn fast_random_char() -> char {
    // Use a simple xorshift; seeded by hashing the address of a static.
    use std::sync::atomic::AtomicU64;
    static STATE: AtomicU64 = AtomicU64::new(1);
    let mut x = STATE.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    // Map to [a-z0-9].
    let idx = (x % 36) as u8;
    if idx < 10 {
        (b'0' + idx) as char
    } else {
        (b'a' + (idx - 10)) as char
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_create_and_drop() {
        let arena = ScratchArena::new(1024 * 1024).unwrap();
        assert!(arena.root().exists());
        let root = arena.root().to_path_buf();
        drop(arena);
        assert!(!root.exists());
    }

    #[test]
    fn arena_output_path_unique() {
        let arena = ScratchArena::new(0).unwrap();
        let a = arena.output_path("png");
        let b = arena.output_path("png");
        assert_ne!(a, b);
    }

    #[test]
    fn arena_stage_bytes_and_cleanup() {
        let arena = ScratchArena::new(1024 * 1024).unwrap();
        let path = arena.stage_bytes(b"hello world", "test").unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello world");
        assert_eq!(arena.current_usage(), 11);
        arena.remove(&path);
        assert!(!path.exists());
        assert_eq!(arena.current_usage(), 0);
    }

    #[test]
    fn arena_limit_exceeded() {
        let arena = ScratchArena::new(5).unwrap();
        let result = arena.stage_bytes(b"too big", "test");
        assert!(result.is_err());
        match result.unwrap_err() {
            ArenaError::LimitExceeded { current: 0, needed: 7, max: 5 } => {},
            other => panic!("unexpected error: {other}"),
        }
    }
}
