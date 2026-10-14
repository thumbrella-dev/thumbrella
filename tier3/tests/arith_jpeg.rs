use std::sync::{Arc, Mutex};

use image::{DynamicImage, ImageFormat, RgbImage};
use tier1::{ResultSource, ResultStatus, ThumbMedia, ThumbResult, ThumbTrace};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ARITHMETIC_JPEG: &[u8] = &[
    255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 1, 0, 72, 0, 72, 0, 0, 255, 219, 0, 67, 0, 8,
    6, 6, 7, 6, 5, 8, 7, 7, 7, 9, 9, 8, 10, 12, 20, 13, 12, 11, 11, 12, 25, 18, 19, 15, 20, 29, 26, 31,
    30, 29, 26, 28, 28, 32, 36, 46, 39, 32, 34, 44, 35, 28, 28, 40, 55, 41, 44, 48, 49, 52, 52, 52, 31,
    39, 57, 61, 56, 50, 60, 46, 51, 52, 50, 255, 201, 0, 11, 8, 0, 48, 0, 64, 1, 1, 17, 0, 255, 204, 0,
    6, 0, 16, 16, 5, 255, 218, 0, 8, 1, 1, 0, 0, 63, 0, 210, 195, 203, 60, 74, 199, 187, 44, 217, 216,
    208, 185, 250, 34, 232, 118, 128, 249, 199, 203, 37, 13, 70, 206, 211, 158, 116, 187, 214, 19, 102,
    117, 93, 212, 171, 113, 85, 89, 52, 196, 128, 207, 255, 217,
];

struct Server {
    url: String,
    handoffs: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(source: Vec<u8>, mime: &'static str) -> Server {
    let mut thumbnail = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::new(32, 24))
        .write_to(&mut thumbnail, ImageFormat::Jpeg)
        .unwrap();
    let extension = match mime {
        "image/jpeg" => "jpeg",
        "image/png" => "png",
        _ => "exr",
    };
    server_with_result(
        source,
        mime,
        ThumbResult {
            status: ResultStatus::Success,
            source: Some(ResultSource::Render),
            media: Some(ThumbMedia {
                kind: tier1::FileKind::Image,
                extension: extension.into(),
                mime: mime.into(),
                thumbnail: thumbnail.into_inner(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
}

async fn server_with_result(source: Vec<u8>, mime: &'static str, result: ThumbResult) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handoffs = Arc::new(Mutex::new(Vec::new()));
    let calls = handoffs.clone();
    let source = Arc::new(source);
    let response = Arc::new(
        serde_json::to_vec(&tier1::handoff::HandoffResponse {
            result,
            trace: ThumbTrace::default(),
        })
        .unwrap(),
    );
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let source = source.clone();
            let response = response.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 8192];
                let header_end = loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
                let len = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|n| n.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while request.len() < header_end + len {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                }
                let (body, content_type) = if headers.starts_with("POST /handoff ") {
                    calls.lock().unwrap().push(headers);
                    (response.as_slice(), "application/json")
                } else {
                    (source.as_slice(), mime)
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                // Header-only requests may close the connection before reading the body.
                let _ = stream.write_all(body).await;
            });
        }
    });
    Server { url, handoffs, task }
}

fn command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_thumbrella"));
    command.kill_on_drop(true);
    for var in [
        "TBR_TIER2", "TBR_TIER3", "TBR_CACHE", "TBR_TRACE", "TBR_HANDSHAKE",
    ] {
        command.env_remove(var);
    }
    command.env("TBR_LOCAL", "true").env("TBR_PIN", "0");
    command
}

async fn result(server: &Server, name: &str, targets: &[(&str, &str)], no_tools: bool) -> ThumbResult {
    let mut command = command();
    command.args(["result", "--raw", &format!("{}/media/{name}", server.url)]);
    for (name, value) in targets {
        command.env(name, value);
    }
    if no_tools {
        command.env("PATH", "");
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stdout)))
}

#[tokio::test]
async fn tier3_handoff_keeps_local_png_renderer() {
    let mut png = std::io::Cursor::new(Vec::new());
    let image = RgbImage::from_fn(1000, 1000, |x, y| {
        image::Rgb([(x * 17 + y * 31) as u8, (x * y) as u8, (x + y * 13) as u8])
    });
    DynamicImage::ImageRgb8(image).write_to(&mut png, ImageFormat::Png).unwrap();
    assert!(png.get_ref().len() > 200 * 1024);
    let server = server(png.into_inner(), "image/png").await;
    for target in ["tbr_s_test_only".to_string(), server.url.clone()] {
        let result = result(&server, "image.png", &[("TBR_TIER3", &target)], true).await;
        assert_eq!(result.source, Some(ResultSource::Render));
        assert_eq!(result.message, None);
        assert!(result.media.unwrap().placeholder.is_empty());
        assert!(server.handoffs.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn exr_handoffs_work_with_either_or_both_targets() {
    let server = server(vec![0x76, 0x2f, 0x31, 0x01, 2, 0, 0, 0], "image/x-exr").await;
    let target = format!("{},tbr_s_test_only", server.url);
    for targets in [
        vec![("TBR_TIER2", target.as_str())],
        vec![("TBR_TIER3", target.as_str())],
        vec![
            ("TBR_TIER2", target.as_str()),
            ("TBR_TIER3", target.as_str()),
        ],
    ] {
        let before = server.handoffs.lock().unwrap().len();
        let result = result(&server, "image.exr", &targets, true).await;
        assert_eq!(result.source, Some(ResultSource::Render));
        assert_eq!(result.message, None);
        assert!(result.media.unwrap().placeholder.is_empty());
        let calls = server.handoffs.lock().unwrap();
        assert_eq!(calls.len(), before + 1);
        assert!(
            calls
                .last()
                .unwrap()
                .to_ascii_lowercase()
                .contains("authorization: bearer tbr_s_test_only")
        );
    }
}

#[tokio::test]
async fn arithmetic_jpeg_reports_missing_graphicsmagick_without_trying_ffmpeg() {
    let mut jpeg = ARITHMETIC_JPEG.to_vec();
    jpeg.resize(1024, 0);
    let server = server(jpeg, "image/jpeg").await;
    let result = result(&server, "image.jpg", &[], true).await;
    assert!(result.message.unwrap().contains("GraphicsMagick (gm)"));
    assert_eq!(result.source, Some(ResultSource::Placeholder));
}

#[tokio::test]
async fn missing_graphicsmagick_can_fall_back_to_a_configured_handoff() {
    let mut jpeg = ARITHMETIC_JPEG.to_vec();
    jpeg.resize(1024, 0);
    let server = server(jpeg, "image/jpeg").await;
    let result = result(&server, "image.jpg", &[("TBR_TIER2", &server.url)], true).await;
    assert_eq!(result.source, Some(ResultSource::Render));
    assert_eq!(result.message, None);
    assert!(result.media.unwrap().placeholder.is_empty());
    assert_eq!(server.handoffs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn arithmetic_jpeg_escalates_past_a_successful_tier2_placeholder() {
    let mut jpeg = ARITHMETIC_JPEG.to_vec();
    jpeg.resize(256 * 1024, 0);
    let placeholder = ThumbResult {
        status: ResultStatus::Success,
        source: Some(ResultSource::Placeholder),
        message: Some("no higher-tier renderer is configured".into()),
        media: Some(ThumbMedia {
            kind: tier1::FileKind::Image,
            extension: "jpeg".into(),
            mime: "image/jpeg".into(),
            placeholder: "image".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let tier2 = server_with_result(jpeg.clone(), "image/jpeg", placeholder).await;
    let tier3 = server(jpeg, "image/jpeg").await;
    let rendered =
        result(&tier2, "image.jpg", &[("TBR_TIER2", &tier2.url), ("TBR_TIER3", &tier3.url)], true)
            .await;
    assert_eq!(rendered.source, Some(ResultSource::Render));
    assert_eq!(rendered.message, None);
    let media = rendered.media.unwrap();
    assert_eq!(media.extension, "jpeg");
    assert!(media.placeholder.is_empty());
    assert!(media.thumbnail.starts_with(&[0xff, 0xd8]));
    assert_eq!(tier2.handoffs.lock().unwrap().len(), 1);
    assert_eq!(tier3.handoffs.lock().unwrap().len(), 1);

    let rendered =
        result(&tier2, "image.jpg", &[("TBR_TIER2", &tier3.url), ("TBR_TIER3", &tier2.url)], true)
            .await;
    assert_eq!(rendered.source, Some(ResultSource::Render));
    assert_eq!(tier2.handoffs.lock().unwrap().len(), 1);
    assert_eq!(tier3.handoffs.lock().unwrap().len(), 2);

    let result = result(&tier2, "image.jpg", &[("TBR_TIER2", &tier2.url)], true).await;
    assert_eq!(result.source, Some(ResultSource::Placeholder));
    assert_eq!(result.message.as_deref(), Some("no higher-tier renderer is configured"));
}

#[tokio::test]
#[ignore = "requires GraphicsMagick (gm) on PATH"]
async fn arithmetic_jpeg_renders_through_a_real_tier3_handoff() {
    let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let base = format!("http://127.0.0.1:{port}");
    let handshake = "arithmetic-handoff-test";
    let mut tier3 = command()
        .env("TBR_PORT", port.to_string())
        .env("TBR_HANDSHAKE", handshake)
        .env("TBR_LOG", "minimal")
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .arg("serve")
        .spawn()
        .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if let Some(status) = tier3.try_wait().unwrap() {
                let mut stderr = String::new();
                tier3.stderr.take().unwrap().read_to_string(&mut stderr).await.unwrap();
                panic!("tier3 exited {status}: {stderr}");
            }
            if let Ok(response) =
                client.get(format!("{base}/health")).header("x-tbr-handshake", handshake).send().await
                && response.status().is_success()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("tier3 did not become responsive");

    let mut jpeg = ARITHMETIC_JPEG.to_vec();
    jpeg.resize(256 * 1024, 0);
    let source = server_with_result(
        jpeg,
        "image/jpeg",
        ThumbResult {
            status: ResultStatus::Success,
            source: Some(ResultSource::Placeholder),
            message: Some("no higher-tier renderer is configured".into()),
            media: Some(ThumbMedia {
                kind: tier1::FileKind::Image,
                extension: "jpeg".into(),
                mime: "image/jpeg".into(),
                placeholder: "image".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    let target = format!("{base},x-tbr-handshake={handshake}");
    for targets in [
        vec![("TBR_TIER3", target.as_str())],
        vec![
            ("TBR_TIER2", source.url.as_str()),
            ("TBR_TIER3", target.as_str()),
        ],
    ] {
        let result = result(&source, "image.jpg", &targets, false).await;
        assert_eq!(result.source, Some(ResultSource::Render));
        assert_eq!(result.message, None);
        let media = result.media.unwrap();
        assert_eq!(media.extension, "jpeg");
        assert_eq!(media.properties["width"], 64);
        assert_eq!(media.properties["height"], 48);
        assert!(media.placeholder.is_empty());
        assert!(media.thumbnail.starts_with(&[0xff, 0xd8]));
    }
    assert_eq!(source.handoffs.lock().unwrap().len(), 1);
    tier3.kill().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the optional GraphicsMagick (gm) runtime backend"]
async fn arithmetic_jpeg_uses_existing_graphicsmagick_backend() {
    let mut jpeg = ARITHMETIC_JPEG.to_vec();
    jpeg.resize(1024, 0);
    let server = server(jpeg, "image/jpeg").await;
    let result = result(&server, "image.jpg", &[], false).await;
    assert_eq!(result.source, Some(ResultSource::Render));
    assert_eq!(result.message, None);
    let media = result.media.unwrap();
    assert!(media.placeholder.is_empty());
    assert_eq!(media.properties["width"], 64);
    assert_eq!(media.properties["height"], 48);
}
