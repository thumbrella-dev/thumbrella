//! Build-time swappable HTTP source access.
//!
//! `native-http` uses reqwest in normal server builds.
//! `worker-fetch` is reserved for the future WASM/Workers transport backend.

use std::collections::HashMap;

#[cfg(feature = "native-http")]
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "native-http")]
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct PrefixDownload {
    pub bytes: Vec<u8>,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub stream_finished: bool,
    /// Final URL after following any HTTP redirects.
    /// `None` only in the stub/worker-fetch backend.
    pub final_url: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ConditionalRequest {
    IfNoneMatch(String),
    IfModifiedSince(String),
}

impl PrefixDownload {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|v| v.as_str())
    }
}

#[cfg(feature = "native-http")]
const MAX_CONCURRENT_REQUESTS_PER_HOST: usize = 3;

#[cfg(feature = "native-http")]
fn shared_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // Keep a small reusable idle pool per host to reduce reconnect/reauth churn.
            .pool_max_idle_per_host(MAX_CONCURRENT_REQUESTS_PER_HOST)
            .build()
            .expect("failed to build shared reqwest client")
    })
}

#[cfg(feature = "native-http")]
fn per_host_semaphore(host_key: &str) -> Arc<Semaphore> {
    static HOST_LIMITERS: OnceLock<Mutex<HashMap<String, Arc<Semaphore>>>> = OnceLock::new();
    let map = HOST_LIMITERS.get_or_init(|| Mutex::new(HashMap::new()));

    let mut guard = map.lock().expect("host limiter mutex poisoned");
    guard
        .entry(host_key.to_string())
        .or_insert_with(|| Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS_PER_HOST)))
        .clone()
}

#[cfg(feature = "native-http")]
fn host_key_from_url(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?
        .to_ascii_lowercase();
    let port = parsed.port_or_known_default().unwrap_or(0);
    Ok(format!("{host}:{port}"))
}

#[cfg(feature = "native-http")]
async fn acquire_host_permit(url: &str) -> Result<OwnedSemaphorePermit, String> {
    let host_key = host_key_from_url(url)?;
    let sem = per_host_semaphore(&host_key);
    sem.acquire_owned()
        .await
        .map_err(|_| format!("host limiter closed for {host_key}"))
}

#[cfg(feature = "native-http")]
fn read_file_url(url: &str) -> Result<Vec<u8>, String> {
    // Strip the file:// prefix and decode percent-encoding.
    let path = url.strip_prefix("file://").unwrap_or(url);
    // Simple percent-decode for common cases (spaces, etc.).
    let path = percent_decode_path(path);
    std::fs::read(&path).map_err(|e| format!("failed to read file {path}: {e}"))
}

#[cfg(feature = "native-http")]
fn percent_decode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(char::from(h << 4 | l));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(feature = "native-http")]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(feature = "native-http")]
pub async fn fetch_prefix(
    url: &str,
    max_bytes: usize,
    conditional: Option<&ConditionalRequest>,
) -> Result<PrefixDownload, String> {
    if url.starts_with("file://") {
        let bytes = read_file_url(url)?;
        let prefix = bytes[..bytes.len().min(max_bytes)].to_vec();
        let stream_finished = prefix.len() == bytes.len();
        let mut headers = HashMap::new();
        headers.insert("content-length".to_string(), bytes.len().to_string());
        return Ok(PrefixDownload { bytes: prefix, status: 200, headers, stream_finished, final_url: Some(url.to_string()) });
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http, https and file URLs are supported".into());
    }

    let _host_permit = acquire_host_permit(url).await?;
    let client = shared_http_client();
    let range_end = max_bytes.saturating_sub(1);
    let mut req = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes=0-{range_end}"));
    if let Some(conditional) = conditional {
        req = apply_conditional_header(req, conditional);
    }

    let mut resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status().as_u16();
    // Capture the final URL (after any redirects) before consuming the body.
    let final_url = Some(resp.url().to_string());
    if !resp.status().is_success() && status != 304 {
        return Err(format!("upstream returned status {}", resp.status()));
    }
    let headers = flatten_headers(resp.headers());

    if status == 304 {
        return Ok(PrefixDownload {
            bytes: Vec::new(),
            status,
            headers,
            stream_finished: true,
            final_url,
        });
    }

    let mut bytes = Vec::with_capacity(max_bytes.min(256 * 1024));
    let mut stream_finished = false;

    while bytes.len() < max_bytes {
        let next = resp
            .chunk()
            .await
            .map_err(|e| format!("failed to read response body: {e}"))?;

        let Some(chunk) = next else {
            stream_finished = true;
            break;
        };

        let remaining = max_bytes - bytes.len();
        if chunk.len() <= remaining {
            bytes.extend_from_slice(&chunk);
        } else {
            bytes.extend_from_slice(&chunk[..remaining]);
            break;
        }
    }

    Ok(PrefixDownload {
        bytes,
        status,
        headers,
        stream_finished,
        final_url,
    })
}

#[cfg(feature = "native-http")]
pub async fn fetch_full(url: &str) -> Result<(Vec<u8>, HashMap<String, String>), String> {
    if url.starts_with("file://") {
        let bytes = read_file_url(url)?;
        let mut headers = HashMap::new();
        headers.insert("content-length".to_string(), bytes.len().to_string());
        return Ok((bytes, headers));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http, https and file URLs are supported".into());
    }

    let _host_permit = acquire_host_permit(url).await?;
    let client = shared_http_client();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("upstream returned status {}", resp.status()));
    }

    let headers = flatten_headers(resp.headers());
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?
        .to_vec();

    Ok((bytes, headers))
}

#[cfg(feature = "native-http")]
pub async fn fetch_range(url: &str, start: u64, end_inclusive: u64) -> Result<(Vec<u8>, HashMap<String, String>), String> {
    if url.starts_with("file://") {
        let bytes = read_file_url(url)?;
        let s = start as usize;
        let e = (end_inclusive as usize + 1).min(bytes.len());
        if s >= bytes.len() {
            return Err(format!("range start {start} beyond file length {}", bytes.len()));
        }
        let slice = bytes[s..e].to_vec();
        let mut headers = HashMap::new();
        headers.insert("content-length".to_string(), bytes.len().to_string());
        return Ok((slice, headers));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http, https and file URLs are supported".into());
    }
    if end_inclusive < start {
        return Err("invalid range".into());
    }

    let _host_permit = acquire_host_permit(url).await?;
    let client = shared_http_client();
    let resp = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end_inclusive}"))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("upstream returned status {}", resp.status()));
    }

    let headers = flatten_headers(resp.headers());
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?
        .to_vec();

    Ok((bytes, headers))
}

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

#[cfg(feature = "native-http")]
fn apply_conditional_header(
    req: reqwest::RequestBuilder,
    conditional: &ConditionalRequest,
) -> reqwest::RequestBuilder {
    match conditional {
        ConditionalRequest::IfNoneMatch(v) => req.header(reqwest::header::IF_NONE_MATCH, v),
        ConditionalRequest::IfModifiedSince(v) => req.header(reqwest::header::IF_MODIFIED_SINCE, v),
    }
}

#[cfg(feature = "worker-fetch")]
pub async fn fetch_prefix(
    _url: &str,
    _max_bytes: usize,
    _conditional: Option<&ConditionalRequest>,
) -> Result<PrefixDownload, String> {
    Err("worker-fetch backend is not implemented yet".into())
}

#[cfg(feature = "worker-fetch")]
pub async fn fetch_full(_url: &str) -> Result<(Vec<u8>, HashMap<String, String>), String> {
    Err("worker-fetch backend is not implemented yet".into())
}

#[cfg(feature = "worker-fetch")]
pub async fn fetch_range(
    _url: &str,
    _start: u64,
    _end_inclusive: u64,
) -> Result<(Vec<u8>, HashMap<String, String>), String> {
    Err("worker-fetch backend is not implemented yet".into())
}
