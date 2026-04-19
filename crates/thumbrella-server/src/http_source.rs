//! Build-time swappable HTTP source access.
//!
//! `native-http` uses reqwest in normal server builds.
//! `worker-fetch` is reserved for the future WASM/Workers transport backend.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct PrefixDownload {
    pub bytes: Vec<u8>,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub stream_finished: bool,
}

impl PrefixDownload {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|v| v.as_str())
    }
}

#[cfg(feature = "native-http")]
pub async fn fetch_prefix(url: &str, max_bytes: usize) -> Result<PrefixDownload, String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http and https URLs are supported".into());
    }

    let client = reqwest::Client::new();
    let range_end = max_bytes.saturating_sub(1);
    let mut resp = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes=0-{range_end}"))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("upstream returned status {}", resp.status()));
    }

    let status = resp.status().as_u16();
    let headers = flatten_headers(resp.headers());

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
    })
}

#[cfg(feature = "native-http")]
pub async fn fetch_full(url: &str) -> Result<(Vec<u8>, HashMap<String, String>), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http and https URLs are supported".into());
    }

    let client = reqwest::Client::new();
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
fn flatten_headers(headers: &reqwest::header::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in headers {
        if let Ok(s) = v.to_str() {
            out.insert(k.as_str().to_ascii_lowercase(), s.to_string());
        }
    }
    out
}

#[cfg(feature = "worker-fetch")]
pub async fn fetch_prefix(_url: &str, _max_bytes: usize) -> Result<PrefixDownload, String> {
    Err("worker-fetch backend is not implemented yet".into())
}

#[cfg(feature = "worker-fetch")]
pub async fn fetch_full(_url: &str) -> Result<(Vec<u8>, HashMap<String, String>), String> {
    Err("worker-fetch backend is not implemented yet".into())
}
