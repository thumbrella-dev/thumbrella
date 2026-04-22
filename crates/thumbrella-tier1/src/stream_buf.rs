//! Async paged HTTP stream buffer.
//!
//! Content-agnostic: knows nothing about image formats or thumbrella internals.
//!
//! `StreamBuf` opens and owns its HTTP connection, providing async
//! random-access reads backed by a sparse page cache.
//!
//! ## Design
//!
//! * **Construction** — `StreamBuf::open(url, options)` sends a GET and
//!   returns immediately; zero bytes are read from the body at this point.
//! * **Seek** — `seek(offset)` sets the cursor; no I/O, always free.
//! * **Read** — serves from the page cache or advances the live stream on
//!   demand.  May return fewer bytes than requested at page boundaries.
//! * **Tail fetch** — when the cursor is within the last `TAIL_FETCH_SIZE`
//!   bytes of a file whose length is known, and the gap from the current
//!   stream head exceeds `STREAM_SKIP_THRESHOLD`, a Range request fetches
//!   the tail directly into the page cache instead of streaming through
//!   irrelevant data.
//! * **Streaming mode** — entered once via `enter_streaming_mode()`.  New
//!   data from the HTTP body is no longer stored in the page cache; it flows
//!   through a single rolling chunk buffer.  Already-cached pages remain
//!   readable.  One-way — once entered there is no return.
//! * **Artificial EOF** — `set_eof(len)` makes reads at or past `len` return
//!   empty without evicting any cached pages.  Clearable with `clear_eof()`.

use std::collections::{BTreeMap, HashMap};

/// Page size for the sparse cache (4 KiB).
pub const PAGE_SIZE: usize = 4 * 1024;

/// Gaps from the stream head to the requested cursor larger than this will
/// trigger a tail Range request (when the cursor is near EOF).
const STREAM_SKIP_THRESHOLD: u64 = 500 * 1024;

/// How many bytes to retrieve in a tail Range request (page-aligned).
const TAIL_FETCH_SIZE: u64 = 60 * 1024;

// ---------------------------------------------------------------------------
// Public options
// ---------------------------------------------------------------------------

/// Options passed to [`StreamBuf::open`].
#[derive(Default)]
pub struct FetchOptions {
    /// Extra request headers forwarded verbatim to the server
    /// (e.g. `If-None-Match`, `If-Modified-Since`, auth tokens).
    pub extra_headers: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum StreamBufError {
    /// A network-level failure (connection error, bad status, etc.).
    Network(String),
    /// A backward seek in streaming mode landed in territory that was never
    /// cached.  The caller must not attempt to re-read already-streamed data.
    SeekBehindStream,
}

impl std::fmt::Display for StreamBufError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(s) => write!(f, "network error: {s}"),
            Self::SeekBehindStream => write!(f, "seek behind stream in streaming mode"),
        }
    }
}

impl std::error::Error for StreamBufError {}

// ---------------------------------------------------------------------------
// StreamBuf
// ---------------------------------------------------------------------------

/// Async random-access buffer over an HTTP resource.
#[cfg(feature = "native-http")]
pub struct StreamBuf {
    /// URL retained for tail Range re-requests.
    url: String,

    /// Flattened response headers from the initial GET.
    pub headers: HashMap<String, String>,
    /// HTTP status code of the initial GET response.
    pub status: u16,
    /// Total file size from `Content-Length`, if the server supplied it.
    pub total_len: Option<u64>,
    /// Whether the server advertised `Accept-Ranges: bytes`.
    pub accepts_ranges: bool,

    /// Logical read cursor.  Moved by `seek` and by `read`.
    cursor: u64,
    /// How many bytes of the primary forward stream have been consumed.
    stream_pos: u64,
    /// Cumulative bytes received from the network (stream + any Range fetches).
    bytes_fetched_count: u64,

    /// Sparse page cache keyed by page index (`byte_offset / PAGE_SIZE`).
    pages: BTreeMap<u64, Vec<u8>>,
    /// Live primary HTTP response body.  `None` once the stream is exhausted.
    body: Option<reqwest::Response>,

    /// Artificial EOF: reads at or past this position return `Ok(0)`.
    eof_override: Option<u64>,

    /// Whether streaming mode is active.  One-way — set but never cleared.
    streaming_mode: bool,
    /// In streaming mode, the most recently received HTTP chunk.
    /// NOT stored in `pages`.
    streaming_chunk: Vec<u8>,
    /// Byte offset at which `streaming_chunk` begins.
    streaming_chunk_start: u64,

    /// Set to `true` if a tail Range request was ever issued.
    pub did_tail_fetch: bool,
}

#[cfg(feature = "native-http")]
impl StreamBuf {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Open an HTTP GET to `url`, collect response headers, and return a
    /// `StreamBuf` ready to serve reads.  Zero bytes are read from the body
    /// at this point.
    pub async fn open(url: String, options: FetchOptions) -> Result<Self, StreamBufError> {
        // TODO: replace with shared per-host client from http_source
        let client = reqwest::Client::new();
        let mut req = client.get(&url);
        for (k, v) in &options.extra_headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| StreamBufError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() && status != 304 {
            return Err(StreamBufError::Network(format!(
                "server returned {status}"
            )));
        }

        let total_len = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());

        let accepts_ranges = resp
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);

        let headers = flatten_headers(resp.headers());

        Ok(Self {
            url,
            headers,
            status,
            total_len,
            accepts_ranges,
            cursor: 0,
            stream_pos: 0,
            bytes_fetched_count: 0,
            pages: BTreeMap::new(),
            body: Some(resp),
            eof_override: None,
            streaming_mode: false,
            streaming_chunk: Vec::new(),
            streaming_chunk_start: 0,
            did_tail_fetch: false,
        })
    }

    // -----------------------------------------------------------------------
    // Cursor (all sync — no I/O)
    // -----------------------------------------------------------------------

    /// Set the cursor to `offset`.  No I/O.
    pub fn seek(&mut self, offset: u64) {
        self.cursor = offset;
    }

    /// Current cursor position.
    pub fn stream_position(&self) -> u64 {
        self.cursor
    }

    /// Set the cursor to the start of the file.  No I/O.
    pub fn rewind(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor by `delta` bytes relative to its current position.  No I/O.
    pub fn seek_relative(&mut self, delta: i64) {
        if delta < 0 {
            self.cursor = self.cursor.saturating_sub((-delta) as u64);
        } else {
            self.cursor = self.cursor.saturating_add(delta as u64);
        }
    }

    /// Effective length of the file as seen by readers: the artificial EOF if
    /// one is set, otherwise `total_len` from the server, otherwise `None`.
    pub fn stream_len(&self) -> Option<u64> {
        self.eof_override.or(self.total_len)
    }

    // -----------------------------------------------------------------------
    // Artificial EOF
    // -----------------------------------------------------------------------

    /// Make reads at or past `len` return `Ok(0)`.  Does not evict pages.
    pub fn set_eof(&mut self, len: u64) {
        self.eof_override = Some(len);
    }

    /// Remove the artificial EOF limit.
    pub fn clear_eof(&mut self) {
        self.eof_override = None;
    }

    // -----------------------------------------------------------------------
    // Streaming mode
    // -----------------------------------------------------------------------

    /// Enter streaming mode.  One-way — cannot be reversed.
    ///
    /// After this call new HTTP chunks are no longer stored in the page cache.
    /// Already-cached pages remain readable via normal `read` calls.
    pub fn enter_streaming_mode(&mut self) {
        self.streaming_mode = true;
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming_mode
    }

    // -----------------------------------------------------------------------
    // Network accounting
    // -----------------------------------------------------------------------

    /// Total bytes received from the network so far.
    pub fn bytes_fetched(&self) -> u64 {
        self.bytes_fetched_count
    }

    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------

    /// Read up to `buf.len()` bytes from the cursor into `buf`, advancing the
    /// cursor by the number of bytes returned.
    ///
    /// May return fewer bytes than requested when a page boundary is reached —
    /// this is correct behaviour; callers that need an exact count should loop.
    /// Returns `Ok(0)` at end of stream (real or artificial).
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, StreamBufError> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Artificial / known EOF check.
        if let Some(end) = self.eof_override.or(self.total_len) {
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

    /// Read `len` bytes starting at `offset` into a new `Vec`.
    ///
    /// The cursor is **not** updated (pread semantics).  Returns fewer bytes
    /// only at end of stream.
    pub async fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, StreamBufError> {
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

    // -----------------------------------------------------------------------
    // Internal: cached-mode read
    // -----------------------------------------------------------------------

    async fn read_cached(&mut self, buf: &mut [u8]) -> Result<usize, StreamBufError> {
        let page_index = self.cursor / PAGE_SIZE as u64;
        self.ensure_page_cached(page_index).await?;

        let Some(page) = self.pages.get(&page_index) else {
            return Ok(0);
        };

        let in_page = (self.cursor % PAGE_SIZE as u64) as usize;
        if in_page >= page.len() {
            return Ok(0);
        }

        // Honour artificial / known EOF within the page.
        let available = page.len() - in_page;
        let limit = if let Some(end) = self.eof_override.or(self.total_len) {
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

    /// Ensure `page_index` is present in the cache, fetching it if needed.
    async fn ensure_page_cached(&mut self, page_index: u64) -> Result<(), StreamBufError> {
        if self.pages.contains_key(&page_index) {
            return Ok(());
        }
        // Nothing lives past a known EOF.
        if let Some(total) = self.total_len {
            if page_index * PAGE_SIZE as u64 >= total {
                return Ok(());
            }
        }

        let page_offset = page_index * PAGE_SIZE as u64;

        // Tail fetch: cursor is near EOF and gap is large enough to skip.
        if let Some(total) = self.total_len {
            let in_tail = page_offset >= total.saturating_sub(TAIL_FETCH_SIZE);
            let big_gap = page_offset.saturating_sub(self.stream_pos) > STREAM_SKIP_THRESHOLD;
            if in_tail && big_gap && self.accepts_ranges {
                return self.fetch_tail_into_pages(total).await;
            }
        }

        self.stream_forward_to_page(page_index).await
    }

    // -----------------------------------------------------------------------
    // Internal: streaming-mode read
    // -----------------------------------------------------------------------

    async fn read_streaming(&mut self, buf: &mut [u8]) -> Result<usize, StreamBufError> {
        // 1. Serve from the page cache if this byte range was captured before
        //    streaming mode was entered.
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

        // 2. If the cursor is behind the stream head and not in a cached page,
        //    the caller asked us to re-read already-discarded data.
        if self.cursor < self.stream_pos {
            return Err(StreamBufError::SeekBehindStream);
        }

        // 3. Advance the live stream until we have data at the cursor.
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

            // Pull the next chunk from the body.
            let Some(body) = self.body.as_mut() else {
                return Ok(0); // Stream exhausted
            };
            let chunk = body
                .chunk()
                .await
                .map_err(|e| StreamBufError::Network(e.to_string()))?;
            let Some(chunk) = chunk else {
                self.body = None;
                return Ok(0);
            };

            self.streaming_chunk_start = self.stream_pos;
            self.stream_pos += chunk.len() as u64;
            self.bytes_fetched_count += chunk.len() as u64;
            self.streaming_chunk = chunk.to_vec();
            // Loop — check whether cursor is now within this fresh chunk.
        }
    }

    // -----------------------------------------------------------------------
    // Internal: page storage helpers
    // -----------------------------------------------------------------------

    /// Advance the primary stream, storing arriving data into the page cache,
    /// until `target_page` is present.
    async fn stream_forward_to_page(&mut self, target_page: u64) -> Result<(), StreamBufError> {
        loop {
            if self.pages.contains_key(&target_page) {
                break;
            }
            let Some(body) = self.body.as_mut() else {
                break;
            };
            let chunk = body
                .chunk()
                .await
                .map_err(|e| StreamBufError::Network(e.to_string()))?;
            let Some(chunk) = chunk else {
                self.body = None;
                break;
            };
            let start = self.stream_pos;
            self.store_chunk_at(start, &chunk);
            self.stream_pos += chunk.len() as u64;
            self.bytes_fetched_count += chunk.len() as u64;
        }
        Ok(())
    }

    /// Issue a Range request for the final `TAIL_FETCH_SIZE` bytes of the
    /// file and store them in the page cache.
    async fn fetch_tail_into_pages(&mut self, total_len: u64) -> Result<(), StreamBufError> {
        // Align the start down to a page boundary.
        let raw_start = total_len.saturating_sub(TAIL_FETCH_SIZE);
        let tail_start = (raw_start / PAGE_SIZE as u64) * PAGE_SIZE as u64;
        let tail_end_inclusive = total_len - 1;

        // TODO: use shared per-host client
        let client = reqwest::Client::new();
        let mut resp = client
            .get(&self.url)
            .header(
                reqwest::header::RANGE,
                format!("bytes={tail_start}-{tail_end_inclusive}"),
            )
            .send()
            .await
            .map_err(|e| StreamBufError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(StreamBufError::Network(format!(
                "tail Range request returned {}",
                resp.status()
            )));
        }

        self.did_tail_fetch = true;

        let mut pos = tail_start;
        loop {
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| StreamBufError::Network(e.to_string()))?;
            let Some(chunk) = chunk else { break };
            self.store_chunk_at(pos, &chunk);
            self.bytes_fetched_count += chunk.len() as u64;
            pos += chunk.len() as u64;
        }

        Ok(())
    }

    /// Split `bytes` across page boundaries and insert into `self.pages`.
    ///
    /// Already-populated byte ranges are skipped (protects against duplicate
    /// chunks during stream advancement).
    fn store_chunk_at(&mut self, mut offset: u64, bytes: &[u8]) {
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            let page_index = offset / PAGE_SIZE as u64;
            let in_page = (offset % PAGE_SIZE as u64) as usize;
            let page = self.pages.entry(page_index).or_default();

            // Skip bytes we already have.
            if in_page < page.len() {
                let skip = (page.len() - in_page).min(bytes.len() - cursor);
                cursor += skip;
                offset += skip as u64;
                continue;
            }

            // Pad to `in_page` for non-aligned Range starts (page-aligned
            // tail fetches should never hit this, but guard defensively).
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "native-http")]
fn flatten_headers(headers: &reqwest::header::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in headers {
        if let Ok(s) = v.to_str() {
            out.insert(k.as_str().to_ascii_lowercase(), s.to_string());
        }
    }
    out
}
