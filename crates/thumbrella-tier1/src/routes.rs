//! HTTP route handlers.

use axum::{
    Json,
    extract::RawQuery,
    http::StatusCode,
    response::Html,
};
use serde_json::{json, Value};
use crate::{
    BatchRequest,
    BatchResponse,
    ItemRequest,
    RequestedOps,
    SourceRef,
    app_config,
};

use crate::pipeline;

/// Parse repeated `url=...` query params from a raw query string.
fn parse_url_params(raw: Option<String>) -> Vec<String> {
    let Some(qs) = raw else { return Vec::new() };
    qs.split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key == "url" { Some(percent_decode(value)) } else { None }
        })
        .filter(|v| !v.is_empty())
        .collect()
}

/// Minimal percent-decoder for URL values (handles %XX and + as space).
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(char::from(hi << 4 | lo));
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// GET /health
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// POST /batch
///
/// Synchronous endpoint: waits for all items and returns a JSON array.
/// Streaming (NDJSON / SSE) will be added once the pipeline iterator is solid.
pub async fn batch(
    Json(req): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, (StatusCode, Json<Value>)> {
    let items = req.into_items();
    if items.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "items must not be empty" })),
        ));
    }

    let profile = app_config().thumbnail_profile();
    let mut tasks = Vec::with_capacity(items.len());
    for item in &items {
        tasks.push(pipeline::process_item(item, &profile));
    }

    let item_results = futures::future::join_all(tasks).await;

    Ok(Json(BatchResponse { items: item_results }))
}

/// GET /dev
///
/// Developer helper endpoint that accepts repeated `url` query params and
/// renders a simple HTML page containing thumbnail previews and item stats.
pub async fn dev(RawQuery(raw): RawQuery) -> Html<String> {
    let profile = app_config().thumbnail_profile();
    let urls = parse_url_params(raw);

    if urls.is_empty() {
                let body = r#"<!doctype html>
<html>
<head>
    <meta charset="utf-8" />
    <title>thumbrella /dev</title>
    <style>
        body { font-family: ui-sans-serif, system-ui, sans-serif; margin: 24px; color: #222; }
        code { background: #f4f4f4; padding: 2px 6px; border-radius: 4px; }
    </style>
</head>
<body>
    <h1>thumbrella /dev</h1>
    <p>Pass one or more <code>url</code> params, for example:</p>
    <p><code>/dev?url=http://localhost:8001/small.jpg&url=http://localhost:8001/android.avif</code></p>
</body>
</html>"#;
                return Html(body.to_string());
        }

        let items: Vec<ItemRequest> = urls
            .iter()
            .map(|url| ItemRequest {
                id: None,
                source: SourceRef::Url { url: url.clone() },
                etag: None,
                ops: RequestedOps::default(),
            })
            .collect();

        let mut tasks = Vec::with_capacity(items.len());
        for item in &items {
            tasks.push(pipeline::process_item(item, &profile));
        }

        let results = futures::future::join_all(tasks).await;

        let mut cards = String::new();
        for (idx, (url, result)) in urls.iter().zip(results.iter()).enumerate() {
                let thumb_html = if let Some(bytes) = &result.thumbnail {
                        let b64 = base64_encode(bytes);
                        format!(
                                "<img alt=\"thumb {idx}\" src=\"data:image/jpeg;base64,{b64}\" loading=\"lazy\" />"
                        )
                } else {
                        "<div class=\"missing\">No thumbnail</div>".to_string()
                };

                let source_meta = serde_json::to_string_pretty(&result.source_meta).unwrap_or_else(|_| "null".to_string());
                let media_meta = serde_json::to_string_pretty(&result.media).unwrap_or_else(|_| "null".to_string());
                let full_result = serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string());
                let error = result.error.as_deref().unwrap_or("none");
                let thumb_bytes = result.thumbnail.as_ref().map(|v| v.len()).unwrap_or(0);

                cards.push_str(&format!(
                        r#"<section class="card">
    <h2>Job {job}</h2>
    <p><strong>URL:</strong> <a href="{url_escaped}" target="_blank" rel="noreferrer">{url_escaped}</a></p>
    <p><strong>Thumbnail bytes:</strong> {thumb_bytes}</p>
    <p><strong>Error:</strong> {error_escaped}</p>
    <div class="thumb">{thumb_html}</div>
    <details open>
        <summary>Source metadata</summary>
        <pre>{source_meta_escaped}</pre>
    </details>
    <details>
        <summary>Media metadata</summary>
        <pre>{media_meta_escaped}</pre>
    </details>
    <details>
        <summary>Full item result</summary>
        <pre>{full_result_escaped}</pre>
    </details>
</section>"#,
                        job = idx + 1,
                        url_escaped = html_escape(url),
                        thumb_bytes = thumb_bytes,
                        error_escaped = html_escape(error),
                        thumb_html = thumb_html,
                        source_meta_escaped = html_escape(&source_meta),
                        media_meta_escaped = html_escape(&media_meta),
                        full_result_escaped = html_escape(&full_result),
                ));
        }

        let html = format!(
                r#"<!doctype html>
<html>
<head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>thumbrella /dev</title>
    <style>
        :root {{
            --bg: #f4f6f8;
            --fg: #1e2933;
            --card: #ffffff;
            --line: #d8dee6;
            --muted: #5b6b7b;
        }}
        body {{
            margin: 0;
            background: linear-gradient(180deg, #eef3f8 0%, #f8fafc 60%, #f4f6f8 100%);
            color: var(--fg);
            font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, sans-serif;
        }}
        main {{ max-width: 1100px; margin: 24px auto 32px; padding: 0 16px; }}
        h1 {{ margin: 0 0 8px; font-size: 1.5rem; }}
        .hint {{ margin: 0 0 16px; color: var(--muted); }}
        .grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 14px; }}
        .card {{ background: var(--card); border: 1px solid var(--line); border-radius: 10px; padding: 12px; box-shadow: 0 2px 8px rgba(20,30,40,0.06); }}
        .card h2 {{ margin: 0 0 8px; font-size: 1.05rem; }}
        .thumb {{ margin: 10px 0; border: 1px solid var(--line); border-radius: 8px; padding: 8px; min-height: 120px; display: flex; align-items: center; justify-content: center; background: #fbfcfd; }}
        .thumb img {{ max-width: 100%; height: auto; border-radius: 6px; }}
        .missing {{ color: var(--muted); font-style: italic; }}
        pre {{ background: #0f1720; color: #c7d5e4; padding: 10px; border-radius: 8px; overflow: auto; font-size: 12px; line-height: 1.35; }}
        summary {{ cursor: pointer; font-weight: 600; margin: 8px 0; }}
        a {{ color: #1155aa; }}
        p {{ margin: 6px 0; }}
    </style>
</head>
<body>
    <main>
        <h1>/dev Results</h1>
        <p class="hint">{count} item(s) processed with the normal tier1 pipeline.</p>
        <div class="grid">{cards}</div>
    </main>
</body>
</html>"#,
                count = urls.len(),
                cards = cards,
        );

        Html(html)
}

fn html_escape(value: &str) -> String {
        value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;")
}

fn base64_encode(input: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        let mut i = 0usize;

        while i + 2 < input.len() {
                let b = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
                out.push(TABLE[((b >> 18) & 63) as usize] as char);
                out.push(TABLE[((b >> 12) & 63) as usize] as char);
                out.push(TABLE[((b >> 6) & 63) as usize] as char);
                out.push(TABLE[(b & 63) as usize] as char);
                i += 3;
        }

        match input.len() - i {
                1 => {
                        let b = (input[i] as u32) << 16;
                        out.push(TABLE[((b >> 18) & 63) as usize] as char);
                        out.push(TABLE[((b >> 12) & 63) as usize] as char);
                        out.push_str("==");
                }
                2 => {
                        let b = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
                        out.push(TABLE[((b >> 18) & 63) as usize] as char);
                        out.push(TABLE[((b >> 12) & 63) as usize] as char);
                        out.push(TABLE[((b >> 6) & 63) as usize] as char);
                        out.push('=');
                }
                _ => {}
        }

        out
}