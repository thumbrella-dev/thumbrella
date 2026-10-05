use super::*;
use crate::cache::{CacheBackend, CacheEntry, memory::MemoryCacheBackend, sqlite::SqliteCacheBackend, unix_now_secs};
use crate::http_buf::{ConnectOptions, HttpError};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::LazyLock;

struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
}

#[derive(Default)]
struct Scenario {
    replies: VecDeque<Reply>,
    requests: Vec<Vec<(String, String)>>,
}

static SCENARIOS: LazyLock<Mutex<HashMap<String, Scenario>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

struct MockStream {
    reply: Reply,
}

impl HttpStream for MockStream {
    async fn connect(url: &str, options: &ConnectOptions) -> Result<Self, HttpError> {
        let reply = {
            let mut scenarios = SCENARIOS.lock();
            let scenario = scenarios.get_mut(url)
                .ok_or_else(|| HttpError::Network(format!("unexpected connection to {url}")))?;
            scenario.requests.push(options.headers.clone());
            scenario.replies.pop_front()
                .ok_or_else(|| HttpError::Network(format!("unexpected extra request to {url}")))?
        };
        if let Some(ref gate) = reply.gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok(Self { reply })
    }

    fn status(&self) -> u16 { self.reply.status }
    fn response_headers(&self) -> HashMap<String, String> { self.reply.headers.clone() }
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        if self.reply.body.is_empty() {
            Ok(None)
        } else {
            Ok(Some(std::mem::take(&mut self.reply.body)))
        }
    }
}

fn reply(status: u16, headers: &[(&str, &str)]) -> Reply {
    Reply {
        status,
        headers: headers.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect(),
        body: vec![],
        gate: None,
    }
}

fn register(name: &str, replies: Vec<Reply>) -> String {
    let url = format!("https://{name}.example/image.png");
    SCENARIOS.lock().insert(url.clone(), Scenario { replies: replies.into(), requests: vec![] });
    url
}

fn requests(url: &str) -> Vec<Vec<(String, String)>> {
    SCENARIOS.lock().get(url).unwrap().requests.clone()
}

fn runtime(store: CacheStore) -> Arc<Runtime> {
    Runtime::new(store, Default::default(), None, None,
        Default::default(), Default::default(), None, false,
        5, 60, 60, 1000, 60)
}

fn memory() -> Arc<dyn CacheBackend> {
    Arc::new(MemoryCacheBackend::with_max_entries(100))
}

fn hints(validator: Option<&str>, fresh: bool) -> CacheHints {
    CacheHints {
        expires_at: Some(if fresh { unix_now_secs() + 60 } else { unix_now_secs() - 1 }),
        etag: validator.map(str::to_string),
        ..Default::default()
    }
}

async fn seed(backend: &Arc<dyn CacheBackend>, url: &str, hints: CacheHints) -> CacheEntry {
    let now = unix_now_secs();
    let entry = CacheEntry::new(ThumbMedia {
        cache: hints.encode(60),
        thumbnail: vec![1, 2, 3],
        kind: FileKind::Image,
        extension: "png".into(),
        mime: "image/png".into(),
        ..Default::default()
    }, 73, now + 500, 700, now);
    backend.promote(url.to_string(), entry.clone()).await;
    entry
}

async fn run(url: &str, runtime: Arc<Runtime>, client: Option<CacheHints>) -> ThumbResult {
    let cook = ThumbCook::<MockStream>::from_input(InputSpec {
        url: url.to_string(), cache: client, allow_local: false,
    }, runtime);
    let (result, _, mut after) = cook.run().await;
    for task in after.drain() {
        task.await;
    }
    result
}

#[tokio::test]
async fn fresh_server_and_client_tokens_skip_upstream_requests() {
    let backend = memory();
    let url = register("fresh-server", vec![]);
    seed(&backend, &url, hints(Some("\"A\""), true)).await;
    let rt = runtime(CacheStore::backend_only(backend));
    let result = run(&url, rt, None).await;
    assert_eq!(result.source, Some(ResultSource::Cache));
    assert_eq!(result.media.unwrap().thumbnail, vec![1, 2, 3]);
    assert!(requests(&url).is_empty());

    let url = register("fresh-client", vec![]);
    let client = hints(Some("\"B\""), true);
    let result = run(&url, runtime(CacheStore::none()), Some(client.clone())).await;
    assert_eq!(result.source, Some(ResultSource::NotModified));
    let media = result.media.unwrap();
    assert!(media.thumbnail.is_empty());
    assert_eq!(CacheHints::decode(&media.cache).unwrap().etag, client.etag);
    assert!(requests(&url).is_empty());
}

#[tokio::test]
async fn changed_200_renders_new_content_and_replaces_cache_metadata() {
    let mut response = reply(200, &[("cache-control", "max-age=3600"), ("etag", "\"B\""), ("content-type", "image/png")]);
    let mut bytes = std::io::Cursor::new(vec![]);
    DynamicImage::ImageRgb8(RgbImage::from_pixel(16, 16, image::Rgb([240, 10, 10])))
        .write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    response.body = bytes.into_inner();
    response.headers.insert("content-length".into(), response.body.len().to_string());
    let url = register("changed-200", vec![response]);
    let backend = memory();
    seed(&backend, &url, hints(Some("\"A\""), false)).await;
    let result = run(&url, runtime(CacheStore::backend_only(backend.clone())), None).await;
    assert_eq!(result.status, ResultStatus::Success);
    assert_ne!(result.source, Some(ResultSource::Cache));
    let media = result.media.unwrap();
    assert!(!media.thumbnail.is_empty());
    assert_ne!(media.thumbnail, vec![1, 2, 3]);
    assert_eq!(CacheHints::decode(&media.cache).unwrap().etag.as_deref(), Some("\"B\""));
    let stored = backend.get_entry(&url, 0).await.unwrap();
    assert_eq!(stored.media.thumbnail, media.thumbnail);
    assert_eq!(stored.media.cache, media.cache);
    assert_eq!(stored.fresh_until, CacheHints::decode(&media.cache).unwrap().expires_at.unwrap());
    assert!(requests(&url)[0].contains(&("if-none-match".into(), "\"A\"".into())));
}

#[tokio::test]
async fn matching_304_refreshes_freshness_cost_and_pin_without_rerendering() {
    for (name, backend) in [
        ("refresh-memory", memory()),
        ("refresh-sqlite", Arc::new(SqliteCacheBackend::open(":memory:").unwrap()) as Arc<dyn CacheBackend>),
    ] {
        let url = register(name, vec![reply(304, &[("cache-control", "max-age=3600"), ("age", "3500")])]);
        let original = seed(&backend, &url, hints(Some("\"A\""), false)).await;
        let rt = runtime(CacheStore::backend_only(backend.clone()));
        let before = unix_now_secs();
        let result = run(&url, rt.clone(), None).await;
        assert_eq!(result.source, Some(ResultSource::Cache));
        assert_eq!(result.http_status, Some(304));
        let media = result.media.unwrap();
        assert_eq!(media.thumbnail, original.media.thumbnail);
        let updated = CacheHints::decode(&media.cache).unwrap();
        assert_eq!(updated.etag.as_deref(), Some("\"A\""));
        assert!((before + 100..=unix_now_secs() + 100).contains(&updated.expires_at.unwrap()));
        let stored = backend.get_entry(&url, 0).await.unwrap();
        assert_eq!(stored.media.cache, media.cache);
        assert_eq!(stored.fresh_until, updated.expires_at.unwrap());
        assert_eq!(stored.render_cost, 73);
        assert_eq!(stored.pin_until, original.pin_until);
        assert!(stored.cache_until >= before + 1000);
        assert_eq!(run(&url, rt, None).await.source, Some(ResultSource::Cache));
        assert_eq!(requests(&url).len(), 1);
    }
}

#[tokio::test]
async fn headerless_304_preserves_the_original_token_and_retention() {
    let url = register("headerless", vec![reply(304, &[]), reply(304, &[])]);
    let backend = memory();
    let original = seed(&backend, &url, hints(Some("\"A\""), false)).await;
    let rt = runtime(CacheStore::backend_only(backend.clone()));
    for _ in 0..2 {
        let result = run(&url, rt.clone(), None).await;
        assert_eq!(result.media.unwrap().cache, original.media.cache);
        let stored = backend.get_entry(&url, 0).await.unwrap();
        assert_eq!(stored.cache_until, original.cache_until);
        assert_eq!(stored.fresh_until, original.fresh_until);
        assert_eq!(stored.pin_until, original.pin_until);
        assert_eq!(stored.render_cost, original.render_cost);
    }
    assert_eq!(requests(&url).len(), 2);
}

#[tokio::test]
async fn different_client_validator_never_validates_unrelated_stored_bytes() {
    let backend = memory();
    let url = register("different-validators", vec![reply(304, &[])]);
    seed(&backend, &url, hints(Some("\"A\""), false)).await;
    let result = run(&url, runtime(CacheStore::backend_only(backend)), Some(hints(Some("\"B\""), false))).await;
    assert_eq!(result.media.unwrap().thumbnail, vec![1, 2, 3]);
    assert!(requests(&url)[0].contains(&("if-none-match".into(), "\"A\"".into())));

    let backend = memory();
    let url = register("client-only-validator", vec![reply(304, &[("etag", "\"B\""), ("cache-control", "max-age=60")])]);
    let original = seed(&backend, &url, hints(None, false)).await;
    let rt = runtime(CacheStore::new(backend.clone(), 5));
    let result = run(&url, rt.clone(), Some(hints(Some("\"B\""), false))).await;
    assert_eq!(result.source, Some(ResultSource::NotModified));
    assert!(result.media.unwrap().thumbnail.is_empty());
    assert_eq!(backend.get_entry(&url, 0).await.unwrap().media.cache, original.media.cache);
    assert!(rt.cache.debounce_check(&url, None).is_none());
}

#[tokio::test]
async fn last_modified_and_no_cache_force_conditional_revalidation() {
    let backend = memory();
    let url = register("last-modified", vec![reply(304, &[("cache-control", "no-cache, max-age=3600")]), reply(304, &[])]);
    let lm = "Wed, 01 Jan 2020 00:00:00 GMT";
    seed(&backend, &url, CacheHints {
        expires_at: Some(unix_now_secs() + 3600),
        last_modified: Some(lm.into()),
        no_cache: true,
        ..Default::default()
    }).await;
    let rt = runtime(CacheStore::backend_only(backend));
    for _ in 0..2 {
        let media = run(&url, rt.clone(), None).await.media.unwrap();
        let hints = CacheHints::decode(&media.cache).unwrap();
        assert!(hints.no_cache);
        assert!(!hints.is_fresh());
    }
    assert_eq!(requests(&url).len(), 2);
    assert!(requests(&url)[0].contains(&("if-modified-since".into(), lm.into())));
}

#[tokio::test]
async fn disallowed_caching_on_304_removes_lookup_but_preserves_pin() {
    let backend = memory();
    let url = register("no-store-304", vec![reply(304, &[("cache-control", "no-store")])]);
    let original = seed(&backend, &url, hints(Some("\"A\""), false)).await;
    let result = run(&url, runtime(CacheStore::backend_only(backend.clone())), None).await;
    assert!(result.media.unwrap().cache.is_empty());
    assert!(backend.get_entry(&url, 0).await.is_none());
    let pin = backend.get_pin_entry(&url).await.unwrap();
    assert_eq!(pin.fresh_until, 0);
    assert_eq!(pin.pin_until, original.pin_until);
    assert_eq!(pin.render_cost, 73);
    assert!(pin.media.cache.is_empty());
}

#[tokio::test]
async fn uncacheable_200_replaces_old_pinned_payload_instead_of_leaving_stale_bytes() {
    let mut response = reply(200, &[("cache-control", "no-store"), ("content-type", "image/png")]);
    let mut bytes = std::io::Cursor::new(vec![]);
    DynamicImage::ImageRgb8(RgbImage::from_pixel(16, 16, image::Rgb([0, 240, 10])))
        .write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    response.body = bytes.into_inner();
    response.headers.insert("content-length".into(), response.body.len().to_string());
    let url = register("no-store-200", vec![response]);
    let backend = memory();
    let original = seed(&backend, &url, hints(Some("\"A\""), false)).await;
    let rt = runtime(CacheStore::new(backend.clone(), 5));
    let result = run(&url, rt.clone(), None).await;
    let media = result.media.unwrap();
    assert!(media.cache.is_empty());
    assert_ne!(media.thumbnail, original.media.thumbnail);
    assert!(backend.get(&url, 0).await.is_none());
    let pin = backend.get_pin_entry(&url).await.unwrap();
    assert_eq!(pin.media.thumbnail, media.thumbnail);
    assert_eq!(pin.pin_until, original.pin_until);
    assert_eq!(run(&url, rt, None).await.media.unwrap().thumbnail, media.thumbnail);
    assert_eq!(requests(&url).len(), 1);
}

#[tokio::test]
async fn debounce_precedes_client_and_durable_checks_and_does_not_slide() {
    let url = register("fixed-debounce", vec![]);
    let backend = memory();
    let rt = runtime(CacheStore::new(backend, 1));
    let result = ThumbResult {
        url: url.clone(), status: ResultStatus::Success, source: Some(ResultSource::Render),
        media: Some(ThumbMedia { thumbnail: vec![7], cache: String::new(), ..Default::default() }),
        ..Default::default()
    };
    rt.cache.debounce_store(&url, &result);
    tokio::time::sleep(std::time::Duration::from_millis(650)).await;
    let mut cook = ThumbCook::<MockStream>::from_input(InputSpec {
        url: url.clone(), cache: Some(hints(Some("\"client\""), true)), allow_local: false,
    }, rt.clone());
    cook.default_pin_ttl = 600;
    let (result, _, _) = cook.run().await;
    assert_eq!(result.media.unwrap().thumbnail, vec![7]);
    assert!(requests(&url).is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(650)).await;
    assert!(rt.cache.debounce_check(&url, None).is_none());
}

#[tokio::test]
async fn failed_responses_populate_debounce_and_unsolicited_304_is_rejected() {
    let url = register("failure-debounce", vec![reply(404, &[])]);
    let rt = runtime(CacheStore::debounce_only(5));
    let first = run(&url, rt.clone(), None).await;
    let second = run(&url, rt, None).await;
    assert_eq!(first.status, ResultStatus::Failed);
    assert_eq!(second.status, ResultStatus::Failed);
    assert_eq!(first.message, second.message);
    assert_eq!(requests(&url).len(), 1);

    let url = register("unsolicited", vec![reply(304, &[])]);
    let result = run(&url, runtime(CacheStore::none()), None).await;
    assert_eq!(result.status, ResultStatus::Failed);
    assert!(result.message.unwrap().contains("without a conditional request"));
}

#[tokio::test]
async fn concurrent_revalidation_requests_share_the_final_response() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut response = reply(304, &[("cache-control", "max-age=60")]);
    response.gate = Some(gate.clone());
    let url = register("coalesced-revalidation", vec![response]);
    let backend = memory();
    seed(&backend, &url, hints(Some("\"A\""), false)).await;
    let rt = runtime(CacheStore::new(backend, 5));
    let first = {
        let url = url.clone();
        let rt = rt.clone();
        tokio::spawn(async move { run(&url, rt, None).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while requests(&url).is_empty() {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let second = {
        let url = url.clone();
        tokio::spawn(async move { run(&url, rt, None).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(requests(&url).len(), 1);
    gate.add_permits(1);
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(first.media.unwrap().thumbnail, second.media.unwrap().thumbnail);
    assert_eq!(requests(&url).len(), 1);
}

#[tokio::test]
async fn cancelled_and_intermediate_responses_release_debounce_leaders() {
    let cache = CacheStore::debounce_only(5);
    let (hit, leader) = cache.debounce_lookup("handoff", None).await;
    assert!(hit.is_none());
    drop(leader);
    let (hit, leader) = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        cache.debounce_lookup("handoff", None),
    ).await.unwrap();
    assert!(hit.is_none());
    leader.unwrap().complete(&ThumbResult {
        status: ResultStatus::Intermediate,
        ..Default::default()
    });
    let (hit, leader) = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        cache.debounce_lookup("handoff", None),
    ).await.unwrap();
    assert!(hit.is_none());
    assert!(leader.is_some());
}

#[tokio::test]
async fn aborting_a_cook_releases_its_leader_and_wakes_the_next_request() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut response = reply(304, &[]);
    response.gate = Some(gate);
    let url = register("aborted-revalidation", vec![response, reply(304, &[])]);
    let backend = memory();
    seed(&backend, &url, hints(Some("\"A\""), false)).await;
    let rt = runtime(CacheStore::new(backend, 5));
    let first = {
        let url = url.clone();
        let rt = rt.clone();
        tokio::spawn(async move { run(&url, rt, None).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while requests(&url).is_empty() {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let mut second = Box::pin(run(&url, rt, None));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(second.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    }).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), second).await.unwrap();
    assert_eq!(result.media.unwrap().thumbnail, vec![1, 2, 3]);
    assert_eq!(requests(&url).len(), 2);
}
