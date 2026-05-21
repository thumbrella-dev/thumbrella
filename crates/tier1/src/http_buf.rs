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

#[cfg(target_arch = "wasm32")]
use web_time::Instant;
#[cfg(not(target_arch = "wasm32"))]
use web_time::Instant;

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

    /// Final response URL after redirects, if the backend exposes it.
    ///
    /// Used for redirect-aware cache identity. Backends that cannot provide a
    /// post-redirect URL can return `None`.
    fn final_url(&self) -> Option<String> { None }

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

// ── StreamBound — platform-sensitive trait bound ──────────────────────────────

/// Sealed bound used by [`ThumbCookGeneric::run`].
///
/// On native the in-process renderer requires `Send + 'static` so it can
/// coerce `&mut ThumbCook<S>` to `&mut dyn RenderCook`.  On wasm32 /
/// non-native targets there is only one thread and no `Send` requirement.
///
/// Callers outside tier1 never need to name this trait — it is automatically
/// satisfied by any `HttpStream` impl on each platform.
#[cfg(feature = "native")]
pub trait StreamBound: HttpStream + Send + 'static {}
#[cfg(feature = "native")]
impl<S: HttpStream + Send + 'static> StreamBound for S {}

#[cfg(not(feature = "native"))]
pub trait StreamBound: HttpStream {}
#[cfg(not(feature = "native"))]
impl<S: HttpStream> StreamBound for S {}

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
    /// Cumulative wall-clock time spent awaiting network I/O (excludes connect).
    io_secs_count: f64,

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
        let final_url = stream.final_url().unwrap_or_else(|| url.clone());

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
            url: final_url,
            headers,
            status,
            content_length,
            accepts_ranges,
            stream,
            cursor: 0,
            stream_pos: 0,
            bytes_fetched_count: 0,
            io_secs_count: 0.0,
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
    ///
    /// Returns the first cached page (bytes 0..PAGE_SIZE) if it was read during
    /// this connection, so callers that need to forward a header block to a
    /// higher-tier handoff can hold onto it.  If nothing was read yet the
    /// page was never populated and `None` is returned.  Callers that don't
    /// need the block can simply discard the return value.
    pub async fn close(&mut self) -> Option<Vec<u8>> {
        self.stream.close().await;
        self.pages.remove(&0)
    }

    // ── Accounting ────────────────────────────────────────────────────────────

    /// Total bytes received from the network so far.
    pub fn bytes_fetched(&self) -> u64 {
        self.bytes_fetched_count
    }

    /// Cumulative time (seconds) spent blocked on network I/O, excluding connect.
    pub fn io_secs(&self) -> f64 {
        self.io_secs_count
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
        let page_start = page_index * PAGE_SIZE as u64;
        let page_end   = page_start + PAGE_SIZE as u64;

        // How many bytes we need for this page to be complete.
        let expected_len = if let Some(cl) = self.content_length {
            if page_start >= cl {
                return Ok(()); // past EOF — nothing to fetch
            }
            (cl.min(page_end).saturating_sub(page_start)) as usize
        } else {
            PAGE_SIZE // unknown length: wait for a full page
        };

        // Page is already fully populated — nothing to do.
        let have = self.pages.get(&page_index).map_or(0, |p| p.len());
        if have >= expected_len {
            return Ok(());
        }

        // The stream has already advanced past this page; store_chunk_at will
        // have deposited whatever bytes arrived, so we cannot get any more.
        if self.stream_pos >= page_end {
            return Ok(());
        }

        // Range-fetch shortcut for distant tail pages.
        if let Some(total) = self.content_length {
            let in_tail = page_start >= total.saturating_sub(TAIL_FETCH_SIZE);
            let big_gap = page_start.saturating_sub(self.stream_pos) > STREAM_SKIP_THRESHOLD;
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

        // Check the current streaming chunk BEFORE the SeekBehindStream guard.
        // When a chunk larger than the read buffer arrives, stream_pos jumps
        // past cursor after the first partial read.  Subsequent reads into the
        // same chunk would incorrectly trigger SeekBehindStream without this.
        {
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
        }

        // cursor is neither in the page cache nor in the current chunk.
        // If it's behind the stream head that data is truly gone.
        if self.cursor < self.stream_pos {
            return Err(HttpError::SeekBehindStream);
        }

        // cursor >= stream_pos: fetch the next chunk(s) until cursor lands inside.
        loop {
            let t = Instant::now();
            let chunk = self.stream.next_chunk().await?;
            self.io_secs_count += t.elapsed().as_secs_f64();
            let Some(chunk) = chunk else {
                return Ok(0);
            };
            self.streaming_chunk_start = self.stream_pos;
            self.stream_pos += chunk.len() as u64;
            self.bytes_fetched_count += chunk.len() as u64;
            self.streaming_chunk = chunk;

            let chunk_end = self.streaming_chunk_start + self.streaming_chunk.len() as u64;
            if self.cursor >= self.streaming_chunk_start && self.cursor < chunk_end {
                let in_chunk = (self.cursor - self.streaming_chunk_start) as usize;
                let n = buf.len().min(self.streaming_chunk.len() - in_chunk);
                buf[..n].copy_from_slice(&self.streaming_chunk[in_chunk..in_chunk + n]);
                self.cursor += n as u64;
                return Ok(n);
            }
        }
    }

    // ── Page cache population ─────────────────────────────────────────────────

    async fn stream_forward_to_page(&mut self, target_page: u64) -> Result<(), HttpError> {
        let page_start = target_page * PAGE_SIZE as u64;
        let page_end   = page_start + PAGE_SIZE as u64;
        // How many bytes belong to this page (less than PAGE_SIZE only for the
        // last page of a file with a known Content-Length).
        let expected_len = if let Some(cl) = self.content_length {
            (cl.min(page_end).saturating_sub(page_start)) as usize
        } else {
            PAGE_SIZE
        };
        loop {
            // Done when the page has all its expected bytes …
            let have = self.pages.get(&target_page).map_or(0, |p| p.len());
            if have >= expected_len {
                break;
            }
            // … or when the stream has already advanced past the end of this
            // page (a previous large chunk already deposited all bytes for it).
            if self.stream_pos >= page_end {
                break;
            }
            let t = Instant::now();
            let chunk = self.stream.next_chunk().await?;
            self.io_secs_count += t.elapsed().as_secs_f64();
            let Some(chunk) = chunk else { break };
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
        let t = Instant::now();
        let mut stream = S::connect(&self.url, &opts).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next_chunk().await? {
            bytes.extend_from_slice(&chunk);
        }
        self.io_secs_count += t.elapsed().as_secs_f64();

        self.did_tail_fetch = true;
        self.bytes_fetched_count += bytes.len() as u64;
        self.store_chunk_at(tail_start, &bytes);
        Ok(())
    }

    /// Issue a direct `Range: bytes=start-(start+len-1)` request and return
    /// the raw bytes.
    ///
    /// Unlike `read_at`, this always opens a fresh connection for the range
    /// regardless of the current stream cursor position.  The response is
    /// stored in the page cache so any subsequent `read_at` call that overlaps
    /// the same range is served from cache.
    ///
    /// Use this for shortcut paths that need a large, targeted slice of the
    /// remote file (e.g. the ZIP tail containing the Central Directory and
    /// embedded thumbnail) without streaming the bytes in between.
    pub async fn fetch_range(&mut self, start: u64, len: usize) -> Result<Vec<u8>, HttpError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = start + len as u64 - 1;
        let opts = ConnectOptions {
            headers: vec![("range".into(), format!("bytes={start}-{end}"))],
        };
        let t = Instant::now();
        let mut stream = S::connect(&self.url, &opts).await?;
        let mut bytes = Vec::with_capacity(len);
        while let Some(chunk) = stream.next_chunk().await? {
            bytes.extend_from_slice(&chunk);
        }
        self.io_secs_count += t.elapsed().as_secs_f64();
        self.bytes_fetched_count += bytes.len() as u64;
        self.store_chunk_at(start, &bytes);
        Ok(bytes)
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
///
/// Supports both `http://`/`https://` URLs (via reqwest) and `file://` URLs
/// (read from the local filesystem).  `file://` is useful for local testing
/// via the CLI — pass a path and it is promoted to a `file://` URL before
/// this point.
#[cfg(feature = "native")]
pub struct ReqwestStream {
    status: u16,
    headers: HashMap<String, String>,
    final_url: Option<String>,
    inner: ReqwestStreamInner,
}

#[cfg(feature = "native")]
enum ReqwestStreamInner {
    Http(reqwest::Response),
    /// Lazy file reader: data is read in PAGE_SIZE chunks via `next_chunk()`.
    /// The file handle is already seeked to `start` on construction; `remaining`
    /// tracks how many bytes are left in the requested range.
    File {
        file: std::fs::File,
        remaining: u64,
    },
    /// Error placeholder: file could not be opened (status carries the code).
    /// `next_chunk` returns `Ok(None)` immediately.
    Empty,
}

#[cfg(feature = "native")]
impl HttpStream for ReqwestStream {
    async fn connect(url: &str, options: &ConnectOptions) -> Result<Self, HttpError> {
        // ── file:// — stream from disk ────────────────────────────────────────
        if let Some(path) = url.strip_prefix("file://") {
            // Get file length without reading any content.
            let file_len = match std::fs::metadata(path) {
                Ok(m) => m.len(),
                Err(e) => {
                    let status = match e.kind() {
                        std::io::ErrorKind::NotFound => 404,
                        std::io::ErrorKind::PermissionDenied => 403,
                        _ => 500,
                    };
                    return Ok(Self {
                        status,
                        headers: HashMap::new(),
                        final_url: Some(url.to_string()),
                        inner: ReqwestStreamInner::Empty,
                    });
                }
            };

            // Parse a `Range: bytes=start-end` header so range fetches work
            // (ZIP shortcut, tail-fetch, raw IFD seeks).
            let (range_start, range_end) = parse_file_range(&options.headers, file_len);
            let slice_len = range_end.saturating_sub(range_start);

            let mut file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(e) => return Err(HttpError::Network(format!("file open: {e}"))),
            };
            if range_start > 0 {
                use std::io::Seek;
                file.seek(std::io::SeekFrom::Start(range_start))
                    .map_err(|e| HttpError::Network(format!("file seek: {e}")))?;
            }

            let mut headers = HashMap::new();
            headers.insert("content-length".to_string(), slice_len.to_string());
            // Advertise range support so ZIP shortcut and tail-fetch work.
            headers.insert("accept-ranges".to_string(), "bytes".to_string());

            return Ok(Self {
                status: if range_start > 0 { 206 } else { 200 },
                headers,
                final_url: Some(url.to_string()),
                inner: ReqwestStreamInner::File { file, remaining: slice_len },
            });
        }

        // ── http:// / https:// — reqwest ──────────────────────────────────────
        let client = http_client();

        let mut req = client.get(url);
        for (k, v) in &options.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| HttpError::Network(e.to_string()))?;

        let final_url = Some(resp.url().to_string());
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());

        Ok(Self { status, headers, final_url, inner: ReqwestStreamInner::Http(resp) })
    }

    fn status(&self) -> u16 { self.status }
    fn response_headers(&self) -> HashMap<String, String> { self.headers.clone() }
    fn final_url(&self) -> Option<String> { self.final_url.clone() }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        match &mut self.inner {
            ReqwestStreamInner::Http(resp) => {
                let chunk = resp
                    .chunk()
                    .await
                    .map_err(|e| HttpError::Network(e.to_string()))?;
                Ok(chunk.map(|b| b.to_vec()))
            }
            ReqwestStreamInner::File { file, remaining } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                let chunk_size = PAGE_SIZE.min(*remaining as usize);
                let mut buf = vec![0u8; chunk_size];
                use std::io::Read;
                match file.read(&mut buf) {
                    Ok(0) => Ok(None),
                    Ok(n) => {
                        *remaining -= n as u64;
                        buf.truncate(n);
                        Ok(Some(buf))
                    }
                    Err(e) => Err(HttpError::Network(format!("file read: {e}"))),
                }
            }
            ReqwestStreamInner::Empty => Ok(None),
        }
    }
}

/// Parse a `Range: bytes=start-end` header into a `(start, end)` byte range
/// (end is exclusive).  Falls back to `(0, file_len)` when absent or invalid.
#[cfg(feature = "native")]
fn parse_file_range(headers: &[(String, String)], file_len: u64) -> (u64, u64) {
    if let Some((_, v)) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("range")) {
        if let Some(range) = v.strip_prefix("bytes=") {
            let mut parts = range.splitn(2, '-');
            let start = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let end   = parts.next().and_then(|s| s.parse::<u64>().ok())
                             .map(|e| e + 1)   // Range header end is inclusive
                             .unwrap_or(file_len);
            return (start.min(file_len), end.min(file_len));
        }
    }
    (0, file_len)
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

/// The process-global reqwest client.  One connection pool shared by every request.
#[cfg(feature = "native")]
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

/// Build and store the shared reqwest client.  Call once from [`startup`] before
/// the server starts accepting requests.  Safe to call multiple times — only the
/// first call has any effect.
///
/// [`startup`]: crate::startup::startup
#[cfg(feature = "native")]
pub fn init_http_client() {
    HTTP_CLIENT.get_or_init(build_http_client);
}

/// Return a reference to the shared client, initialising it on first use if
/// [`init_http_client`] was never called (e.g. in the CLI `thumb` subcommand).
#[cfg(feature = "native")]
fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(build_http_client)
}

#[cfg(feature = "native")]
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .http2_adaptive_window(true)
        .tcp_nodelay(true)
        .pool_max_idle_per_host(8)
        .build()
        .expect("failed to build reqwest client")
}

// ── Platform stream alias ─────────────────────────────────────────────────────

/// The HTTP backend used by native server builds.
///
/// Use this alias instead of spelling out `HttpBuffer::<ReqwestStream>` at
/// call sites.  The Workers build provides its own `FetchStream` in a
/// downstream crate and uses `HttpBuffer::<FetchStream>` there.
#[cfg(feature = "native")]
pub type PlatformStream = ReqwestStream;

// ── ReadSeek supertrait ───────────────────────────────────────────────────────

/// Combined `Read + Seek` supertrait, object-safe when used as
/// `Box<dyn ReadSeek + Send>`.
///
/// Rust's trait-object rules permit at most one non-auto trait as the primary
/// bound.  By making `ReadSeek` a single supertrait of both `Read` and `Seek`,
/// callers can write `Box<dyn ReadSeek + Send>` where a type-erased seekable
/// reader is needed.
///
/// A blanket impl covers all `T: Read + Seek`, so `Cursor<Vec<u8>>`,
/// `std::fs::File`, [`SyncHttpReader`], etc. all implement this automatically.
pub trait ReadSeek: std::io::Read + std::io::Seek {}
impl<T: std::io::Read + std::io::Seek> ReadSeek for T {}

// ── SyncHttpReader ────────────────────────────────────────────────────────────

/// Synchronous `std::io::Read + Seek` adapter over an [`HttpBuffer`].
///
/// Bridges the async paged HTTP buffer to blocking callers such as libav's
/// `AVIOContext` read/seek callbacks.  Those callbacks are C function
/// pointers; they cannot `await`, so async I/O must be driven by calling
/// `handle.block_on(future)` on the tokio runtime handle captured at
/// construction time.
///
/// # Usage
///
/// Create a `SyncHttpReader` inside an `async` context so that
/// `Handle::current()` resolves to the active tokio runtime.  Then move the
/// reader into a `spawn_blocking` task (or any blocking thread that holds a
/// valid tokio handle).  Inside the blocking task you can pass a
/// `Box<SyncHttpReader<S>>` wherever `Box<dyn Read + Seek + Send>` is
/// expected.
///
/// ```ignore
/// // In an async function:
/// let reader = SyncHttpReader::new(http_buf);
/// let result = tokio::task::spawn_blocking(move || {
///     decode_with_libav(Box::new(reader), Some(content_length), ext_hint)
/// }).await?;
/// ```
///
/// # Seek from end
///
/// `SeekFrom::End` requires a known `Content-Length`.  If the server did not
/// send a `Content-Length` header the seek returns
/// `ErrorKind::Unsupported` and libav will fall back to probing without
/// seeking, which is generally fine for container formats that store their
/// index at the start of the file.
#[cfg(feature = "native")]
pub struct SyncHttpReader<S: HttpStream> {
    buf: HttpBuffer<S>,
    handle: tokio::runtime::Handle,
}

#[cfg(feature = "native")]
impl<S: HttpStream> SyncHttpReader<S> {
    /// Wrap `buf` in a sync adapter.  Must be called from an async context
    /// (i.e. while a tokio runtime is active on the current thread).
    pub fn new(buf: HttpBuffer<S>) -> Self {
        Self {
            buf,
            handle: tokio::runtime::Handle::current(),
        }
    }

    /// Return the `Content-Length` reported by the server, if any.
    pub fn content_length(&self) -> Option<u64> {
        self.buf.content_length
    }

    /// Consume and return the underlying [`HttpBuffer`].
    pub fn into_inner(self) -> HttpBuffer<S> {
        self.buf
    }
}

#[cfg(feature = "native")]
impl<S: HttpStream> std::io::Read for SyncHttpReader<S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.handle
            .block_on(self.buf.read(out))
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

#[cfg(feature = "native")]
impl<S: HttpStream> std::io::Seek for SyncHttpReader<S> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match pos {
            std::io::SeekFrom::Start(n) => {
                self.buf.seek(n);
                Ok(n)
            }
            std::io::SeekFrom::Current(delta) => {
                self.buf.seek_relative(delta);
                Ok(self.buf.stream_position())
            }
            std::io::SeekFrom::End(delta) => {
                let len = self.buf.stream_len().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "content-length unknown, cannot seek from end",
                    )
                })?;
                let pos = if delta < 0 {
                    len.saturating_sub((-delta) as u64)
                } else {
                    len.saturating_add(delta as u64)
                };
                self.buf.seek(pos);
                Ok(pos)
            }
        }
    }
}
