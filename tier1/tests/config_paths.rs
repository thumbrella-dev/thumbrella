#![cfg(feature = "native")]

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use tier1::check::{CheckReport, ValidationStatus};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("tbr-paths-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tier1"));
        command
            .env_clear()
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("TBR_LOCAL", "1")
            .env("TBR_LOG", "minimal")
            .env("NO_COLOR", "1")
            .env("TBR_PORT", "0");
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

struct TestServer(Child);

impl Drop for TestServer {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

#[tokio::test]
async fn pin_and_placeholder_requests_are_logged_with_best_effort_redirect_deduplication() {
    use tier1::cache::CacheBackend;

    let directory = TestDirectory::new();
    let database = directory.0.join("cache.db");
    let backend = tier1::cache::sqlite::SqliteCacheBackend::open(database.to_str().unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    backend
        .put(
            "source".into(),
            tier1::ThumbMedia {
                kind: tier1::FileKind::Image,
                thumbnail: vec![0xff, 0xd8, 0xff, 0xd9],
                ..Default::default()
            },
            0,
            now + 60,
            60,
        )
        .await;
    let id = backend.issue_pin(tier1::FileKind::Image, "source", 60).await.unwrap().unwrap();
    drop(backend);
    let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let log_path = directory.0.join("server.log");
    let error_path = directory.0.join("server.err");
    let mut server = TestServer(
        directory
            .command()
            .env("TBR_CACHE", format!("sqlite:{}", database.display()))
            .env("TBR_PORT", port.to_string())
            .env("TBR_HANDSHAKE", "logging-test-secret")
            .env("TBR_LOG", "standard")
            .stdout(Stdio::from(std::fs::File::create(&log_path).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&error_path).unwrap()))
            .arg("serve")
            .spawn()
            .unwrap(),
    );
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        if let Some(status) = server.0.try_wait().unwrap() {
            panic!("server exited {status}: {}", std::fs::read_to_string(&error_path).unwrap());
        }
        if let Ok(response) = client
            .get(format!("{base}/health"))
            .header("x-tbr-handshake", "logging-test-secret")
            .send()
            .await
        {
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        ready,
        "server did not respond: {}",
        std::fs::read_to_string(&error_path).unwrap()
    );
    let hit = client.get(format!("{base}/pin/{id}.jpeg")).send().await.unwrap();
    assert_eq!(hit.status(), reqwest::StatusCode::OK);
    let miss = client.get(format!("{base}/pin/v000000000000.jpeg")).send().await.unwrap();
    assert_eq!(miss.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(miss.headers()["location"], "/placeholder/video.jpeg");
    let malformed = client.get(format!("{base}/pin/bad")).send().await.unwrap();
    assert_eq!(malformed.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(!malformed.headers().contains_key("location"));
    for referrer in [
        None,
        Some(format!("{base}/pin/v000000000000.jpeg")),
        Some("http://elsewhere.example/pin/v000000000000.jpeg".into()),
        Some(format!("{base}/health")),
    ] {
        let mut request = client.get(format!("{base}/placeholder/video.jpeg"));
        if let Some(referrer) = referrer {
            request = request.header("referer", referrer);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), tier1::assets::placeholders::VIDEO);
    }
    let invalid = client
        .get(format!("{base}/placeholder/no-suffix"))
        .header("referer", format!("{base}/pin/bad"))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::NOT_FOUND);
    drop(server);
    let logs = std::fs::read_to_string(&log_path).unwrap();
    let line = |prefix: &str| logs.lines().find(|line| line.starts_with(prefix)).unwrap().to_string();
    assert!(line(&format!("GET /pin/{id}.jpeg ")).contains("  200"));
    assert!(line("GET /pin/v000000000000.jpeg ").contains("  307  redirect to /placeholder/video.jpeg"));
    assert!(line("GET /pin/bad ").contains("  404"));
    assert!(!line("GET /pin/bad ").contains("redirect"));
    assert_eq!(
        logs.lines()
            .filter(|line| line.starts_with("GET /placeholder/video.jpeg "))
            .count(),
        3,
        "{logs}"
    );
    assert!(line("GET /placeholder/no-suffix ").contains("  404"));
}

#[test]
fn expanded_cache_and_trace_paths_are_opened_and_reported_consistently() {
    let directory = TestDirectory::new();
    let data = directory.0.join("data+comma,$LITERAL%name%");
    std::fs::create_dir(&data).unwrap();
    let mut command = directory.command();
    command
        .env("TBR_PATH_TEST_DATA", &data)
        .env("TBR_CACHE", "mem:10+sqlite:${TBR_PATH_TEST_DATA}/cache.db,1mb")
        .env("TBR_TRACE", "ndjson:~/trace.ndjson")
        .env("TBR_SCRATCH", "${TBR_PATH_TEST_DATA}/scratch");
    let source = url::Url::from_file_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/placeholders/image.jpeg"),
    )
    .unwrap();
    let output = command.args(["result", "--raw", source.as_str()]).output().unwrap();
    assert_success(&output);
    assert!(data.join("cache.db").is_file());
    assert!(directory.0.join("trace.ndjson").is_file());

    let output = directory
        .command()
        .env("TBR_PATH_TEST_DATA", &data)
        .env("TBR_CACHE", "mem:10+sqlite:${TBR_PATH_TEST_DATA}/cache.db,1mb")
        .env("TBR_TRACE", "ndjson:~/trace.ndjson")
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert_success(&output);
    let report: CheckReport = serde_json::from_slice(&output.stdout).unwrap();
    let cache_path = format!("{}/cache.db", data.display());
    let trace_path = format!("{}/trace.ndjson", directory.0.display());
    assert_eq!(report.cache_file_check.unwrap().path, cache_path);
    assert_eq!(report.trace_file_check.unwrap().path, trace_path);
    assert!(report.cache_config.unwrap().contains(&cache_path));
}

#[test]
fn missing_variables_are_explicit_configuration_errors() {
    let directory = TestDirectory::new();
    let output = directory
        .command()
        .env("TBR_CACHE", "sqlite:$TBR_PATH_TEST_UNSET.db")
        .env("TBR_TRACE", "ndjson:${TBR_PATH_TEST_UNSET}.ndjson")
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: CheckReport = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.cache_validation.status, ValidationStatus::Error);
    assert_eq!(report.trace_validation.status, ValidationStatus::Error);
    assert!(report.cache_validation.message.unwrap().contains("TBR_PATH_TEST_UNSET"));
    assert!(report.trace_validation.message.unwrap().contains("TBR_PATH_TEST_UNSET"));
    assert!(report.cache_file_check.is_none());
    assert!(report.trace_file_check.is_none());

    let output = directory
        .command()
        .env("TBR_SCRATCH", "$TBR_PATH_TEST_UNSET")
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("TBR_SCRATCH"), "{stderr}");
    assert!(stderr.contains("TBR_PATH_TEST_UNSET"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn non_unicode_scratch_configuration_is_not_silently_ignored() {
    use std::os::unix::ffi::OsStringExt;

    let directory = TestDirectory::new();
    let output = directory
        .command()
        .env("TBR_SCRATCH", std::ffi::OsString::from_vec(vec![0xff]))
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TBR_SCRATCH"));
}

#[test]
fn invalid_pin_secret_makes_cli_check_fail_without_rotating_it() {
    let directory = TestDirectory::new();
    let path = directory.0.join("cache.db");
    drop(tier1::cache::sqlite::SqliteCacheBackend::open(path.to_str().unwrap()).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE thumbrella_metadata SET value = ?1 WHERE name = 'pin_secret'",
        [vec![42u8; 31]],
    )
    .unwrap();
    drop(conn);
    let output = directory
        .command()
        .env("TBR_CACHE", format!("sqlite:{}", path.display()))
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    let report: CheckReport = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!report.healthy);
    assert_eq!(
        report.cache_file_check.unwrap().sqlite_validation.unwrap().status,
        ValidationStatus::Error
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    let length: i64 = conn
        .query_row(
            "SELECT length(value) FROM thumbrella_metadata WHERE name = 'pin_secret'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(length, 31);
}

#[test]
fn pin_identity_survives_sqlite_process_restarts_but_not_memory_restarts() {
    let directory = TestDirectory::new();
    let source = url::Url::from_file_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/placeholders/image.jpeg"),
    )
    .unwrap();
    let request = |dsn: &str| {
        let output = directory
            .command()
            .env("TBR_CACHE", dsn)
            .args(["result", "--raw", source.as_str()])
            .output()
            .unwrap();
        assert_success(&output);
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let pin = result["pin"].as_str().unwrap().to_string();
        let id = pin.strip_prefix("pin/").unwrap().strip_suffix(".jpeg").unwrap();
        assert!(tier1::cache::pins::valid_pin(id));
        pin
    };
    let persistent = format!("mem:10+sqlite:{}", directory.0.join("cache.db").display());
    assert_eq!(request(&persistent), request(&persistent));
    assert_ne!(request("mem:10"), request("mem:10"));
}

#[test]
fn result_returns_a_real_candidate_pin_for_a_missing_handler_without_storing_the_placeholder() {
    let directory = TestDirectory::new();
    let source = directory.0.join("unsupported.svg");
    std::fs::write(&source, r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32"/></svg>"#).unwrap();
    let source_url = url::Url::from_file_path(&source).unwrap();
    let database = directory.0.join("cache.db");
    let dsn = format!("sqlite:{}", database.display());
    let output = directory
        .command()
        .env("TBR_CACHE", dsn)
        .env("TBR_PIN", "60")
        .args(["result", "--raw", source_url.as_str()])
        .output()
        .unwrap();
    assert_success(&output);
    let result: tier1::ThumbResult = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result.source, Some(tier1::result::ResultSource::Placeholder));
    let media = result.media.as_ref().unwrap();
    assert!(!media.placeholder.is_empty());
    let backend = tier1::cache::sqlite::SqliteCacheBackend::open(database.to_str().unwrap()).unwrap();
    use tier1::cache::CacheBackend;
    let id = backend.pin_candidate(media.kind, source_url.as_str(), 0).unwrap();
    assert_eq!(result.pin.as_deref(), Some(format!("pin/{id}.jpeg").as_str()));
    let conn = rusqlite::Connection::open(&database).unwrap();
    for table in ["thumbrella", "thumbrella_pins"] {
        let count: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "{table} should not store this placeholder or its pin");
    }
}

#[test]
fn check_reports_pin_default_explicit_ttl_and_disabled_states() {
    let directory = TestDirectory::new();
    for (pin, cache_max, ttl, default, enabled) in [
        (None, None, 604_800, true, true),
        (None, Some("86400"), 86_400, true, true),
        (Some(""), None, 604_800, true, true),
        (Some("  "), None, 604_800, true, true),
        (Some("3600"), None, 3_600, false, true),
        (Some(" 60 "), None, 60, false, true),
        (Some("1"), None, 1, false, true),
        (Some("4294967295"), None, u64::from(u32::MAX), false, true),
        (Some("0"), None, 0, false, false),
        (None, Some("0"), 0, true, false),
    ] {
        for json in [true, false] {
            let mut command = directory.command();
            if let Some(pin) = pin {
                command.env("TBR_PIN", pin);
            }
            if let Some(cache_max) = cache_max {
                command.env("TBR_CACHE_MAX_TTL", cache_max);
            }
            command.arg("check");
            if json {
                command.arg("--json");
            }
            let output = command.output().unwrap();
            assert_success(&output);
            if json {
                let report: CheckReport = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(report.pin_ttl_secs, ttl);
                assert_eq!(report.pin_default, default);
                assert_eq!(report.pin_enabled, enabled);
                assert_eq!(report.pin_validation.status, ValidationStatus::Ok);
                assert!(report.healthy);
            } else {
                let text = String::from_utf8_lossy(&output.stdout);
                let line = text.lines().find(|line| line.starts_with("TBR_PIN:")).unwrap();
                assert!(line.contains(&format!("{ttl} seconds")), "{line}");
                assert_eq!(line.contains("(default)"), default, "{line}");
                assert!(
                    line.contains(if enabled { "(enabled)" } else { "(pinning disabled)" }),
                    "{line}"
                );
            }
        }
    }
}

#[test]
fn invalid_pin_times_fail_json_and_text_checks_and_runtime_startup() {
    let directory = TestDirectory::new();
    for value in ["-1", "1.5", "5d", "true", "4294967296", "18446744073709551616"] {
        let output = directory
            .command()
            .env("TBR_PIN", value)
            .args(["check", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{value}");
        let report: CheckReport = serde_json::from_slice(&output.stdout).unwrap();
        assert!(!report.healthy);
        assert!(!report.pin_enabled);
        assert!(!report.pin_default);
        assert_eq!(report.pin_validation.status, ValidationStatus::Error);
        let message = report.pin_validation.message.unwrap();
        assert!(
            message.contains(value) && message.contains("whole number of seconds"),
            "{message}"
        );

        let output = directory.command().env("TBR_PIN", value).arg("check").output().unwrap();
        assert!(!output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("TBR_PIN: invalid") && text.contains(value), "{text}");
        assert!(!text.contains("pinning disabled"), "{text}");

        let output = directory
            .command()
            .env("TBR_PIN", value)
            .args(["result", "--raw", "https://example.com/image.jpg"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("TBR_PIN") && error.contains("whole number of seconds"),
            "{error}"
        );
        assert!(output.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_pin_time_is_reported_as_invalid_instead_of_defaulting() {
    use std::os::unix::ffi::OsStringExt;

    let directory = TestDirectory::new();
    let output = directory
        .command()
        .env("TBR_PIN", std::ffi::OsString::from_vec(vec![0xff]))
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: CheckReport = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.pin_validation.status, ValidationStatus::Error);
    assert!(!report.pin_default);
    assert!(!report.healthy);
}
