#![cfg(feature = "native")]

use std::path::PathBuf;
use std::process::{Command, Output};
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
