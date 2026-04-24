//! Async paged HTTP buffer — `HttpBuffer<S: HttpStream>`.
//!
//! ## Architecture
//!
//! The HTTP backend is abstracted behind the [`HttpStream`] trait.  The
//! buffer's page cache, cursor, and EOF logic are generic over any backend.
//! Different backends compile in for different build targets:
//!
//! | Backend          | Type              | Where defined             |
//! |------------------|-------------------|---------------------------|
//! | reqwest (native) | [`ReqwestStream`] | this module, `native` feature |
//! | workers-rs fetch | `FetchStream`     | downstream workers crate  |
//!
//! Typical usage in the native server:
//! ```ignore
//! let buf = HttpBuffer::<ReqwestStream>::open(url, ConnectOptions::default()).await?;
//! ```
//!
//! A workers crate implements [`HttpStream`] for its own fetch type and uses
//! `HttpBuffer::<FetchStream>` with no changes here.
//!
//! ## HttpBuffer features
//!
//! * **Seek** — `seek(offset)` is always free; no I/O.
//! * **Paged cache** — network chunks stored in 4 KiB pages; re-reads served
//!   from cache.
//! * **Tail fetch** — sparse Range request for the file tail when the cursor
//!   jumps far ahead of the stream and file length is known.
//! * **Streaming mode** — new chunks bypass the page cache and flow through a
//!   rolling buffer.  Already-cached pages remain readable.  One-way.
//! * **Artificial EOF** — limits the visible file length without evicting pages.

use std::collections::{BTreeMap, HashMap};

/// Page size for the sparse cache (4 KiB).
pub const PAGE_SIZE: usize = 4 * 1024;

/// Stream-to-cursor gaps larger than this trigger a tail Range fetch.
const STREAM_SKIP_THRESHOLD: u64 = 500 * 1024;

/// Bytes to retrieve in a tail Range request (page-aligned).
const TAIL_FETCH_SIZE: u64 = 60 * 1024;

// ── Options ───────────────────────────────────────────────────────────────────

/// Options forwarded to the HTTP backend when opening a connection.
#[derive(Default)]
pub struct ConnectOptions {
    /// Request headers sent verbatim (e.g. `If-None-Match`, `Range`, auth tokens).
    pub headers: Vec<(String, String)>,
}

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum HttpError {
    /// Network-level failure (connection error, non-success status, etc.).
    Network(String),
    /// Backward seek in streaming mode into already-discarded data.
    SeekBehindStream,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(s) => write!(f, "network error: {s}"),
            Self::SeekBehindStream => write!(f, "seek behind stream in streaming mode"),
        }
    }
}

impl std::error::Error for HttpError {}

// ── HttpStream trait ──────────────────────────────────────────────────────────

/// Minimal interface required from an HTTP backend.
///
/// Implement this for a new runtime (reqwest, workers-rs fetch, test stubs,
/// …) and `HttpBuffer<YourStream>` works without changes to this module.
///
/// The three methods the buffer depends on are:
/// - `connect` — open a GET (or Range GET, via headers) and return the stream.
/// - `next_chunk` — pull the next bytes from the response body.
/// - `close` — release the connection on early exit.
///
/// The two metadata accessors (`status`, `response_headers`) are read once
/// after `connect` and cached in `HttpBuffer`.  `content_length` and
/// `accepts_ranges` are derived from those headers inside `HttpBuffer::open`
/// so backends don't need to parse them.
///
/// # Note on `async fn` in traits
///
/// This trait uses `async fn` (stable since Rust 1.75).  It is intentionally
/// not object-safe — `dyn HttpStream` is not a goal; the backend is always
/// chosen at compile time via generics, so the `Send` bound omission is fine.
#[allow(async_fn_in_trait)]
pub trait HttpStream: Sized {
    /// Open an HTTP GET for `url` and return the connected stream.
    async fn connect(url: &str, options: &ConnectOptions) -> Result<Self, HttpError>;

    /// HTTP status code of the initial response.
    fn status(&self) -> u16;

    /// Flattened response headers (lowercase keys).
    fn response_headers(&self) -> HashMap<String, String>;

    /// Pull the next chunk of bytes from the response body.
    /// Returns `Ok(None)` when the stream is exhausted.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError>;

    /// Release the underlying connection before the body is fully consumed.
    ///
    /// Call this whenever `HttpBuffer` is discarded mid-stream — e.g. after
    /// a shortcut or early-exit path.  Backends that own a `ReadableStream`
    /// (Cloudflare Workers) must override this to call `cancel()`; for
    /// native reqwest the default no-op is sufficient because dropping the
    /// response body signals the connection pool automatically.
    async fn close(&mut self) {}
}

// ── HttpBuffer ────────────────────────────────────────────────────────────────

/// Async random-access buffer over an HTTP resource.
///
/// Generic over any [`HttpStream`] backend — `HttpBuffer<ReqwestStream>` for
/// the native server, `HttpBuffer<FetchStream>` in a workers crate, etc.
pub struct HttpBuffer<S: HttpStream> {
    /// URL of the resource — retained for tail Range re-requests.
    pub url: String,
    /// Flattened response headers captured on open.
    pub headers: HashMap<String, String>,
    /// HTTP status code of the initial response.
    pub status: u16,
    /// `Content-Length` from the server, if provided.
    pub content_length: Option<u64>,
    /// `true` if the server supports byte-range requests.
    pub accepts_ranges: bool,

    /// The HTTP backend — the only backend-specific value in this struct.
    stream: S,

    /// Logical read cursor.
    cursor: u64,
    /// Bytes consumed from the primary forward stream so far.
    stream_pos: u64,
    /// Cumulative bytes received from the network (stream + Range fetches).
    bytes_fetched_count: u64,

    /// Sparse page cache keyed by page index (`byte_offset / PAGE_SIZE`).
    pages: BTreeMap<u64, Vec<u8>>,

    /// Artificial EOF: reads at or past this offset return `Ok(0)`.
    eof_override: Option<u64>,

    /// `true` once streaming mode has been entered (one-way).
    streaming_mode: bool,
    /// In streaming mode, the most recently received chunk (not in `pages`).
    streaming_chunk: Vec<u8>,
    /// Byte offset at which `streaming_chunk` begins.
    streaming_chunk_start: u64,

    /// `true` if a tail Range request was ever issued.
    pub did_tail_fetch: bool,
}

impl<S: HttpStream> HttpBuffer<S> {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Open an HTTP GET to `url` and return a buffer ready for reads.
    /// Zero bytes are pulled from the body at this point.
    pub async fn open(url: String, options: ConnectOptions) -> Result<Self, HttpError> {
        let stream = S::connect(&url, &options).await?;

        let status = stream.status();
        let headers = stream.response_headers();

        let content_length = headers
            .get("content-length")
            .and_then(|v| v.parse().ok());
        let accepts_ranges = headers
            .get("accept-ranges")
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);

        Ok(Self {
            url,
            headers,
            status,
            content_length,
            accepts_ranges,
            stream,
            cursor: 0,
            stream_pos: 0,
            bytes_fetched_count: 0,
            pages: BTreeMap::new(),
            eof_override: None,
            streaming_mode: false,
            streaming_chunk: Vec::new(),
            streaming_chunk_start: 0,
            did_tail_fetch: false,
        })
    }

    // ── Cursor ────────────────────────────────────────────────────────────────

    /// Set the cursor to `offset`.  No I/O.
    pub fn seek(&mut self, offset: u64) {
        self.cursor = offset;
    }

    /// Current cursor position.
    pub fn stream_position(&self) -> u64 {
        self.cursor
    }

    /// Reset the cursor to the start.  No I/O.
    pub fn rewind(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor by `delta` bytes.  No I/O.
    pub fn seek_relative(&mut self, delta: i64) {
        if delta < 0 {
            self.cursor = self.cursor.saturating_sub((-delta) as u64);
        } else {
            self.cursor = self.cursor.saturating_add(delta as u64);
        }
    }

    /// Effective file length: artificial EOF if set, else `content_length`, else `None`.
    pub fn stream_len(&self) -> Option<u64> {
        self.eof_override.or(self.content_length)
    }

    // ── Artificial EOF ────────────────────────────────────────────────────────

    /// Make reads at or past `len` return `Ok(0)`.  Does not evict pages.
    pub fn set_eof(&mut self, len: u64) {
        self.eof_override = Some(len);
    }

    pub fn clear_eof(&mut self) {
        self.eof_override = None;
    }

    // ── Streaming mode ────────────────────────────────────────────────────────

    /// Enter streaming mode (one-way).  New chunks bypass the page cache.
    pub fn enter_streaming_mode(&mut self) {
        self.streaming_mode = true;
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming_mode
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    /// Release the underlying connection before the body is fully consumed.
    ///
    /// Must be called on any early-exit path (type unsupported, shortcut found,
    /// error, etc.).  When the stream is fully drained this is a no-op.
    pub async fn close(&mut self) {
        self.stream.close().await;
    }

    // ── Accounting ────────────────────────────────────────────────────────────

    /// Total bytes received from the network so far.
    pub fn bytes_fetched(&self) -> u64 {
        self.bytes_fetched_count
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    /// Read up to `buf.len()` bytes from the cursor into `buf`.
    ///
    /// May return fewer bytes than requested at a page boundary — callers
    /// needing an exact count should loop.  Returns `Ok(0)` at end of stream.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, HttpError> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(end) = self.eof_override.or(self.content_length) {
            if self.cursor >= end {
                return Ok(0);
            }
        }
        if self.streaming_mode {
            self.read_streaming(buf).await
        } else {
            self.read_cached(buf).await
        }
    }

    /// Read `len` bytes starting at `offset` without moving the cursor
    /// (pread semantics).
    pub async fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, HttpError> {
        let saved = self.cursor;
        self.cursor = offset;
        let mut out = Vec::with_capacity(len);
        let mut tmp = vec![0u8; PAGE_SIZE];
        while out.len() < len {
            let want = (len - out.len()).min(tmp.len());
            let n = self.read(&mut tmp[..want]).await?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&tmp[..n]);
        }
        self.cursor = saved;
        Ok(out)
    }

    // ── Cached-mode read ──────────────────────────────────────────────────────

    async fn read_cached(&mut self, buf: &mut [u8]) -> Result<usize, HttpError> {
        let page_index = self.cursor / PAGE_SIZE as u64;
        self.ensure_page_cached(page_index).await?;

        let Some(page) = self.pages.get(&page_index) else {
            return Ok(0);
        };
        let in_page = (self.cursor % PAGE_SIZE as u64) as usize;
        if in_page >= page.len() {
            return Ok(0);
        }

        let available = page.len() - in_page;
        let limit = if let Some(end) = self.eof_override.or(self.content_length) {
            (end.saturating_sub(self.cursor) as usize).min(available)
        } else {
            available
        };

        let n = buf.len().min(limit);
        if n == 0 {
            return Ok(0);
        }
        buf[..n].copy_from_slice(&page[in_page..in_page + n]);
        self.cursor += n as u64;
        Ok(n)
    }

    async fn ensure_page_cached(&mut self, page_index: u64) -> Result<(), HttpError> {
        if self.pages.contains_key(&page_index) {
            return Ok(());
        }
        if let Some(total) = self.content_length {
            if page_index * PAGE_SIZE as u64 >= total {
                return Ok(());
            }
        }
        let page_offset = page_index * PAGE_SIZE as u64;
        if let Some(total) = self.content_length {
            let in_tail = page_offset >= total.saturating_sub(TAIL_FETCH_SIZE);
            let big_gap = page_offset.saturating_sub(self.stream_pos) > STREAM_SKIP_THRESHOLD;
            if in_tail && big_gap && self.accepts_ranges {
                return self.fetch_tail_into_pages(total).await;
            }
        }
        self.stream_forward_to_page(page_index).await
    }

    // ── Streaming-mode read ───────────────────────────────────────────────────

    async fn read_streaming(&mut self, buf: &mut [u8]) -> Result<usize, HttpError> {
        // Serve from cache if this range was captured before streaming mode.
        let page_index = self.cursor / PAGE_SIZE as u64;
        if let Some(page) = self.pages.get(&page_index) {
            let in_page = (self.cursor % PAGE_SIZE as u64) as usize;
            if in_page < page.len() {
                let n = buf.len().min(page.len() - in_page);
                buf[..n].copy_from_slice(&page[in_page..in_page + n]);
                self.cursor += n as u64;
                return Ok(n);
            }
        }

        if self.cursor < self.stream_pos {
            return Err(HttpError::SeekBehindStream);
        }

        loop {
            let chunk_end = self.streaming_chunk_start + self.streaming_chunk.len() as u64;
            if !self.streaming_chunk.is_empty()
                && self.cursor >= self.streaming_chunk_start
                && self.cursor < chunk_end
            {
                let in_chunk = (self.cursor - self.streaming_chunk_start) as usize;
                let n = buf.len().min(self.streaming_chunk.len() - in_chunk);
                buf[..n].copy_from_slice(&self.streaming_chunk[in_chunk..in_chunk + n]);
                self.cursor += n as u64;
                return Ok(n);
            }

            let Some(chunk) = self.stream.next_chunk().await? else {
                return Ok(0);
            };
            self.streaming_chunk_start = self.stream_pos;
            self.stream_pos += chunk.len() as u64;
            self.bytes_fetched_count += chunk.len() as u64;
            self.streaming_chunk = chunk;
        }
    }

    // ── Page cache population ─────────────────────────────────────────────────

    async fn stream_forward_to_page(&mut self, target_page: u64) -> Result<(), HttpError> {
        loop {
            if self.pages.contains_key(&target_page) {
                break;
            }
            let Some(chunk) = self.stream.next_chunk().await? else {
                break;
            };
            let start = self.stream_pos;
            self.store_chunk_at(start, &chunk);
            self.stream_pos += chunk.len() as u64;
            self.bytes_fetched_count += chunk.len() as u64;
        }
        Ok(())
    }

    async fn fetch_tail_into_pages(&mut self, content_length: u64) -> Result<(), HttpError> {
        let raw_start = content_length.saturating_sub(TAIL_FETCH_SIZE);
        let tail_start = (raw_start / PAGE_SIZE as u64) * PAGE_SIZE as u64;

        let opts = ConnectOptions {
            headers: vec![(
                "range".into(),
                format!("bytes={tail_start}-{}", content_length - 1),
            )],
        };
        let mut stream = S::connect(&self.url, &opts).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next_chunk().await? {
            bytes.extend_from_slice(&chunk);
        }

        self.did_tail_fetch = true;
        self.bytes_fetched_count += bytes.len() as u64;
        self.store_chunk_at(tail_start, &bytes);
        Ok(())
    }

    fn store_chunk_at(&mut self, mut offset: u64, bytes: &[u8]) {
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            let page_index = offset / PAGE_SIZE as u64;
            let in_page = (offset % PAGE_SIZE as u64) as usize;
            let page = self.pages.entry(page_index).or_default();

            if in_page < page.len() {
                let skip = (page.len() - in_page).min(bytes.len() - cursor);
                cursor += skip;
                offset += skip as u64;
                continue;
            }
            if page.len() < in_page {
                page.resize(in_page, 0);
            }
            let space = PAGE_SIZE - page.len();
            let take = space.min(bytes.len() - cursor);
            page.extend_from_slice(&bytes[cursor..cursor + take]);
            cursor += take;
            offset += take as u64;
        }
    }
}

// ── ReqwestStream — native backend ───────────────────────────────────────────

/// reqwest-backed [`HttpStream`] implementation for the native server.
///
/// Use as `HttpBuffer::<ReqwestStream>::open(url, options)`.
/// The workers crate provides its own `FetchStream` without touching this.
#[cfg(feature = "native")]
pub struct ReqwestStream {
    status: u16,
    headers: HashMap<String, String>,
    body: reqwest::Response,
}

#[cfg(feature = "native")]
impl HttpStream for ReqwestStream {
    async fn connect(url: &str, options: &ConnectOptions) -> Result<Self, HttpError> {
        // TODO: replace with a shared per-host connection pool
        let client = reqwest::Client::new();
        let mut req = client.get(url);
        for (k, v) in &options.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| HttpError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() && status != 304 {
            return Err(HttpError::Network(format!("server returned {status}")));
        }

        let headers = flatten_headers(resp.headers());

        Ok(Self { status, headers, body: resp })
    }

    fn status(&self) -> u16 { self.status }
    fn response_headers(&self) -> HashMap<String, String> { self.headers.clone() }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        let chunk = self
            .body
            .chunk()
            .await
            .map_err(|e| HttpError::Network(e.to_string()))?;
        Ok(chunk.map(|b| b.to_vec()))
    }
}

#[cfg(feature = "native")]
fn flatten_headers(headers: &reqwest::header::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in headers {
        if let Ok(s) = v.to_str() {
            out.insert(k.as_str().to_ascii_lowercase(), s.to_string());
        }
    }
    out
}
