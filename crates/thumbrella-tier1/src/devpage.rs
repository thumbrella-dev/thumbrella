use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::time::Instant;
use v_htmlescape::escape;

use crate::{
    ItemRequest,
    RequestRecord,
    RequestedOps,
    SourceRef,
    app_config,
    pipeline,
};

const DEV_FORM_TEMPLATE: &str = include_str!("templates/dev_form.html");
const DEV_CARD_TEMPLATE: &str = include_str!("templates/dev_card.html");
const DEV_RESULTS_TEMPLATE: &str = include_str!("templates/dev_results.html");

pub async fn render(urls: Vec<String>, record: &mut RequestRecord, start: Instant) -> String {
    if urls.is_empty() {
        return DEV_FORM_TEMPLATE.to_string();
    }

    let profile = app_config().thumbnail_profile();
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
        tasks.push(pipeline::process_item(item, &profile, &record.id));
    }

    let results = futures::future::join_all(tasks).await;
    record.duration_secs = Some(start.elapsed().as_secs_f64());

    let mut cards = String::new();
    for (idx, (url, result)) in urls.iter().zip(results.iter()).enumerate() {
        let thumb_html = if let Some(bytes) = &result.thumbnail {
            let b64 = STANDARD.encode(bytes);
            format!(
                "<img alt=\"thumb {idx}\" src=\"data:image/jpeg;base64,{b64}\" \
                 width=\"{}\" height=\"{}\" loading=\"lazy\" \
                 style=\"image-rendering:auto;display:block\" />",
                profile.width, profile.height
            )
        } else {
            "<div class=\"missing\">No thumbnail</div>".to_string()
        };

        let mut api_result = result.clone().into_api();
        api_result.thumbnail = None;

        let mut public_result = api_result.clone();
        let server_record = public_result.server.take();
        let public_result_json = serde_json::to_string_pretty(&public_result)
            .unwrap_or_else(|_| "{}".to_string());
        let server_record_json = match server_record {
            Some(record) => serde_json::to_string_pretty(&record)
                .unwrap_or_else(|_| "{}".to_string()),
            None => {
                "{\n  \"note\": \"no server record for this item\"\n}"
                    .to_string()
            }
        };
        let error = result.error.as_deref().unwrap_or("none");
        let thumb_bytes = result.thumbnail.as_ref().map(|value| value.len()).unwrap_or(0);

        let job = (idx + 1).to_string();
        let thumb_bytes = thumb_bytes.to_string();
        let url_escaped = escape(url).to_string();
        let error_escaped = escape(error).to_string();
        let public_result_escaped = escape(&public_result_json).to_string();
        let server_record_escaped = escape(&server_record_json).to_string();

        cards.push_str(&render_template(
            DEV_CARD_TEMPLATE,
            &[
                ("__JOB__", &job),
                ("__URL_ESCAPED__", &url_escaped),
                ("__THUMB_BYTES__", &thumb_bytes),
                ("__ERROR_ESCAPED__", &error_escaped),
                ("__THUMB_HTML__", &thumb_html),
                ("__PUBLIC_RESULT_ESCAPED__", &public_result_escaped),
                ("__SERVER_RECORD_ESCAPED__", &server_record_escaped),
            ],
        ));
    }

    let count = urls.len().to_string();
    let request_json = serde_json::to_string_pretty(record)
        .unwrap_or_else(|_| "{}".to_string());
    let request_escaped = escape(&request_json).to_string();
    let html = render_template(
        DEV_RESULTS_TEMPLATE,
        &[
            ("__COUNT__", &count),
            ("__CARDS__", &cards),
            ("__REQUEST_ESCAPED__", &request_escaped),
        ],
    );

    html
}

fn render_template(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (from, to) in replacements {
        out = out.replace(from, to);
    }
    out
}