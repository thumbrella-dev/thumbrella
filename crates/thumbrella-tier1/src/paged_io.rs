//! Paged source buffer with a blocking `Read + Seek` facade.
//!
//! This module is transport-agnostic: HTTP and local-file feeders can both
//! populate pages while consumers use normal blocking file semantics.
//! Current policy is intentionally simple: retain all fetched pages in memory
//! until decode/processing is complete. Future work can add format-aware page
//! pinning and read-once streaming hints.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};

pub const DEFAULT_PAGE_SIZE: usize = 64 * 1024;
pub const DEFAULT_READAHEAD_PAGES: usize = 4;

type PageRequestHook = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[derive(Clone)]
pub struct PageCache {
    inner: Arc<PageCacheInner>,
}

struct PageCacheInner {
    page_size: usize,
    readahead_pages: usize,
    state: Mutex<PageState>,
    cv: Condvar,
    request_hook: Option<PageRequestHook>,
}

#[derive(Debug)]
struct PageState {
    total_len: Option<u64>,
    terminal_error: Option<String>,
    eof: bool,
    pages: BTreeMap<u64, Vec<u8>>,
    in_flight: HashSet<u64>,
}

impl Default for PageState {
    fn default() -> Self {
        Self {
            total_len: None,
            terminal_error: None,
            eof: false,
            pages: BTreeMap::new(),
            in_flight: HashSet::new(),
        }
    }
}

impl PageCache {
    pub fn new(page_size: usize, readahead_pages: usize, request_hook: Option<PageRequestHook>) -> Self {
        let page_size = page_size.max(1);
        let readahead_pages = readahead_pages.max(1);
        Self {
            inner: Arc::new(PageCacheInner {
                page_size,
                readahead_pages,
                state: Mutex::new(PageState::default()),
                cv: Condvar::new(),
                request_hook,
            }),
        }
    }

    pub fn with_defaults(request_hook: Option<PageRequestHook>) -> Self {
        Self::new(DEFAULT_PAGE_SIZE, DEFAULT_READAHEAD_PAGES, request_hook)
    }

    pub fn page_size(&self) -> usize {
        self.inner.page_size
    }

    pub fn set_total_len(&self, total_len: u64) {
        let mut state = self.inner.state.lock().expect("page cache lock poisoned");
        state.total_len = Some(total_len);
        self.inner.cv.notify_all();
    }

    pub fn mark_eof(&self) {
        let mut state = self.inner.state.lock().expect("page cache lock poisoned");
        state.eof = true;
        self.inner.cv.notify_all();
    }

    pub fn fail_terminal(&self, error: impl Into<String>) {
        let mut state = self.inner.state.lock().expect("page cache lock poisoned");
        state.terminal_error = Some(error.into());
        self.inner.cv.notify_all();
    }

    pub fn insert_page(&self, page_index: u64, bytes: Vec<u8>) {
        if bytes.len() > self.inner.page_size {
            return;
        }

        let mut state = self.inner.state.lock().expect("page cache lock poisoned");
        state.in_flight.remove(&page_index);
        state.pages.insert(page_index, bytes);
        self.inner.cv.notify_all();
    }

    pub fn clear_in_flight(&self, page_index: u64) {
        let mut state = self.inner.state.lock().expect("page cache lock poisoned");
        state.in_flight.remove(&page_index);
        self.inner.cv.notify_all();
    }

    /// Drop all cached source pages immediately.
    ///
    /// Call this as soon as decode has produced the final in-memory image
    /// buffer so downloaded source bytes do not linger.
    pub fn discard_all_pages(&self) {
        let mut state = self.inner.state.lock().expect("page cache lock poisoned");
        state.pages.clear();
        state.in_flight.clear();
        self.inner.cv.notify_all();
    }

    /// Reset all source-tracking state after decode completion.
    ///
    /// This is a convenience for ending one decode lifecycle and preparing
    /// the cache object for a new source.
    pub fn reset_after_decode(&self) {
        let mut state = self.inner.state.lock().expect("page cache lock poisoned");
        state.pages.clear();
        state.in_flight.clear();
        state.total_len = None;
        state.eof = false;
        state.terminal_error = None;
        self.inner.cv.notify_all();
    }

    /// Returns whether a page currently exists in the cache.
    ///
    /// This is primarily useful for tests and diagnostics while eviction is
    /// intentionally disabled in the initial implementation.
    pub fn has_page(&self, page_index: u64) -> bool {
        let state = self.inner.state.lock().expect("page cache lock poisoned");
        state.pages.contains_key(&page_index)
    }

    pub fn reader(&self) -> BlockingPageReader {
        BlockingPageReader {
            cache: self.clone(),
            pos: 0,
        }
    }

    fn request_pages_for(&self, state: &mut PageState, pos: u64, requested_len: usize) -> Vec<(u64, u64)> {
        let page_size_u64 = self.inner.page_size as u64;
        let first_page = pos / page_size_u64;
        let requested_pages = requested_len
            .div_ceil(self.inner.page_size)
            .max(1)
            .max(self.inner.readahead_pages);

        let mut requests = Vec::new();
        for i in 0..requested_pages {
            let page = first_page + i as u64;
            if let Some(total_len) = state.total_len {
                let page_start = page * page_size_u64;
                if page_start >= total_len {
                    break;
                }
            }

            if state.pages.contains_key(&page) || state.in_flight.contains(&page) {
                continue;
            }

            state.in_flight.insert(page);
            let start = page * page_size_u64;
            let end = start + page_size_u64;
            requests.push((start, end));
        }

        requests
    }

    fn copy_available(&self, state: &PageState, pos: u64, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }

        let Some(mut remaining) = state
            .total_len
            .map(|len| len.saturating_sub(pos) as usize)
            .or(Some(out.len()))
        else {
            return 0;
        };

        if remaining == 0 {
            return 0;
        }

        let mut copied = 0usize;
        let page_size = self.inner.page_size;

        while copied < out.len() && remaining > 0 {
            let absolute = pos + copied as u64;
            let page_index = absolute / page_size as u64;
            let in_page_offset = (absolute % page_size as u64) as usize;

            let Some(page) = state.pages.get(&page_index) else {
                break;
            };

            if in_page_offset >= page.len() {
                break;
            }

            let page_remaining = page.len() - in_page_offset;
            let want = (out.len() - copied).min(page_remaining).min(remaining);
            out[copied..copied + want]
                .copy_from_slice(&page[in_page_offset..in_page_offset + want]);
            copied += want;
            remaining -= want;
        }

        copied
    }

    fn has_missing_before_known_eof(&self, state: &PageState, pos: u64) -> bool {
        if let Some(total_len) = state.total_len {
            if pos >= total_len {
                return false;
            }
        }

        let page_index = pos / self.inner.page_size as u64;
        !state.pages.contains_key(&page_index)
    }

    fn wait_for_progress(&self, pos: u64, requested_len: usize) -> io::Result<()> {
        let mut state = self.inner.state.lock().expect("page cache lock poisoned");
        loop {
            if let Some(err) = &state.terminal_error {
                return Err(io::Error::other(err.clone()));
            }

            if !self.has_missing_before_known_eof(&state, pos) {
                return Ok(());
            }

            if state.eof {
                return Ok(());
            }

            let requests = self.request_pages_for(&mut state, pos, requested_len);
            if let Some(request_hook) = self.inner.request_hook.clone() {
                for (start, end) in requests {
                    request_hook(start, end);
                }
            }

            state = self.inner.cv.wait(state).expect("page cache lock poisoned");
        }
    }
}

pub struct BlockingPageReader {
    cache: PageCache,
    pos: u64,
}

impl Read for BlockingPageReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            let state = self.cache.inner.state.lock().expect("page cache lock poisoned");
            if let Some(err) = &state.terminal_error {
                return Err(io::Error::other(err.clone()));
            }

            let copied = self.cache.copy_available(&state, self.pos, buf);
            if copied > 0 {
                self.pos = self.pos.saturating_add(copied as u64);
                return Ok(copied);
            }

            if state.total_len.is_some_and(|len| self.pos >= len) {
                return Ok(0);
            }

            if state.eof && !self.cache.has_missing_before_known_eof(&state, self.pos) {
                return Ok(0);
            }

            drop(state);
            self.cache.wait_for_progress(self.pos, buf.len())?;
        }
    }
}

impl Seek for BlockingPageReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let state = self.cache.inner.state.lock().expect("page cache lock poisoned");
        let new_pos = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(delta) => self.pos as i128 + delta as i128,
            SeekFrom::End(delta) => {
                let Some(total_len) = state.total_len else {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "seek from end requires known total length",
                    ));
                };
                total_len as i128 + delta as i128
            }
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }

        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn blocking_reader_waits_for_pages() {
        let cache = PageCache::new(4, 1, None);
        cache.set_total_len(8);

        let feeder = cache.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            feeder.insert_page(0, b"abcd".to_vec());
            thread::sleep(Duration::from_millis(20));
            feeder.insert_page(1, b"efgh".to_vec());
            feeder.mark_eof();
        });

        let mut reader = cache.reader();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read should succeed");
        assert_eq!(out, b"abcdefgh");
    }

    #[test]
    fn seek_from_end_requires_known_length() {
        let cache = PageCache::new(4, 1, None);
        let mut reader = cache.reader();
        let err = reader
            .seek(SeekFrom::End(0))
            .expect_err("seek from end should fail without length");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn pages_are_retained_after_read() {
        let cache = PageCache::new(4, 1, None);
        cache.set_total_len(8);
        cache.insert_page(0, b"abcd".to_vec());
        cache.insert_page(1, b"efgh".to_vec());
        cache.mark_eof();

        let mut reader = cache.reader();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read should succeed");
        assert_eq!(out, b"abcdefgh");

        // Initial policy: no eviction while decode is active.
        assert!(cache.has_page(0));
        assert!(cache.has_page(1));
    }

    #[test]
    fn discard_all_pages_releases_cached_data() {
        let cache = PageCache::new(4, 1, None);
        cache.set_total_len(8);
        cache.insert_page(0, b"abcd".to_vec());
        cache.insert_page(1, b"efgh".to_vec());
        assert!(cache.has_page(0));
        assert!(cache.has_page(1));

        cache.discard_all_pages();
        assert!(!cache.has_page(0));
        assert!(!cache.has_page(1));
    }

    #[test]
    fn reset_after_decode_clears_length_and_errors() {
        let cache = PageCache::new(4, 1, None);
        cache.set_total_len(8);
        cache.insert_page(0, b"abcd".to_vec());
        cache.fail_terminal("boom");
        cache.mark_eof();

        cache.reset_after_decode();
        assert!(!cache.has_page(0));
        cache.set_total_len(0);

        let mut reader = cache.reader();
        let mut buf = [0u8; 1];
        // No terminal error should remain after reset.
        let res = reader.read(&mut buf).expect("read should not return terminal error");
        assert_eq!(res, 0);
    }
}
