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
    register_path(name, "image.png", replies)
}

fn register_path(name: &str, path: &str, replies: Vec<Reply>) -> String {
    let url = format!("https://{name}.example/{path}");
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
    Arc::new(MemoryCacheBackend::with_max_entries(100).unwrap())
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

struct CountedPinBackend {
    backend: Arc<dyn CacheBackend>,
    calls: std::sync::atomic::AtomicUsize,
}

impl CacheBackend for CountedPinBackend {
    fn name(&self) -> &'static str { self.backend.name() }

    fn pin_candidate(&self, kind: FileKind, key: &str, attempt: u32) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.pin_candidate(kind, key, attempt)
    }

    fn get_entry<'a>(&'a self, key: &'a str, pin_ttl: u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<CacheEntry>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.get_entry(key, pin_ttl)
    }

    fn get_pin_entry<'a>(&'a self, key: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<CacheEntry>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.get_pin_entry(key)
    }

    fn claim_pin<'a>(&'a self, id: &'a str, key: &'a str, ttl: u64) -> crate::cache::PinFuture<'a, crate::cache::pins::PinClaim> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.claim_pin(id, key, ttl)
    }

    fn pin_thumbnail<'a>(&'a self, id: &'a str) -> crate::cache::PinFuture<'a, Option<Vec<u8>>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.pin_thumbnail(id)
    }

    fn promote(&self, key: String, entry: CacheEntry) -> crate::after::DeferredFuture {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.promote(key, entry)
    }

    fn revalidate(&self, key: String, entry: CacheEntry, expected_cache: String) -> crate::after::DeferredFuture {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.revalidate(key, entry, expected_cache)
    }

    fn put(&self, key: String, media: ThumbMedia, cost: u8, cache_until: u64, pin_ttl: u64) -> crate::after::DeferredFuture {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.put(key, media, cost, cache_until, pin_ttl)
    }
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

struct CapabilityRenderer {
    available: bool,
    calls: std::sync::atomic::AtomicUsize,
}

impl crate::renderer::InProcessRenderer for CapabilityRenderer {
    fn render<'a>(&'a self, cook: &'a mut dyn RenderCook) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if !self.available {
                cook.note("external tool unavailable (test)");
                return false;
            }
            cook.set_render_image(DynamicImage::ImageRgb8(
                RgbImage::from_pixel(16, 16, image::Rgb([240, 10, 10])),
            ));
            cook.set_render_renderer("test".into());
            true
        })
    }
}

fn vector_reply() -> Reply {
    let mut response = reply(200, &[("cache-control", "max-age=60"), ("content-type", "image/svg+xml")]);
    response.body = br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#.to_vec();
    response
}

fn capability_renderer(available: bool) -> Arc<CapabilityRenderer> {
    Arc::new(CapabilityRenderer { available, calls: Default::default() })
}

#[tokio::test]
async fn missing_local_handlers_keep_client_and_debounce_caches_and_allow_upgrades() {
    for registered in [false, true] {
        let url = register_path(&format!("missing-local-{registered}"), "image.svg", vec![vector_reply(), vector_reply()]);
        let backend = memory();
        let mut rt = runtime(CacheStore::new(backend.clone(), 5));
        if registered {
            Arc::make_mut(&mut rt).renderer = Some(capability_renderer(false));
        }
        let before = unix_now_secs();
        let first = run(&url, rt.clone(), None).await;
        assert_eq!(first.source, Some(ResultSource::Placeholder));
        let media = first.media.unwrap();
        assert_eq!(media.placeholder, "vector");
        let client = CacheHints::decode(&media.cache).unwrap();
        assert!((before + 86400..=unix_now_secs() + 86400).contains(&client.expires_at.unwrap()));
        assert!(backend.get_entry(&url, 0).await.is_none());
        assert_eq!(run(&url, rt, None).await.source, Some(ResultSource::Cache));
        assert_eq!(requests(&url).len(), 1);
        let fresh = run(&url, runtime(CacheStore::none()), Some(client)).await;
        assert_eq!(fresh.source, Some(ResultSource::NotModified));
        assert_eq!(requests(&url).len(), 1);

        let mut upgraded = runtime(CacheStore::new(backend.clone(), 5));
        Arc::make_mut(&mut upgraded).renderer = Some(capability_renderer(true));
        let rendered = run(&url, upgraded, None).await;
        assert_eq!(rendered.source, Some(ResultSource::Render));
        assert!(rendered.media.as_ref().unwrap().placeholder.is_empty());
        assert_eq!(backend.get_entry(&url, 0).await.unwrap().media.thumbnail, rendered.media.unwrap().thumbnail);
        assert_eq!(requests(&url).len(), 2);
    }
}

#[tokio::test]
async fn existing_cached_formats_do_not_require_a_current_handler() {
    let url = register_path("cached-unavailable", "image.svg", vec![]);
    let backend = memory();
    let mut entry = seed(&backend, &url, hints(None, true)).await;
    entry.media.kind = FileKind::Vector;
    entry.media.extension = "svg".into();
    backend.promote(url.clone(), entry).await;
    let renderer = capability_renderer(false);
    let mut rt = runtime(CacheStore::backend_only(backend));
    Arc::make_mut(&mut rt).renderer = Some(renderer.clone());
    let result = run(&url, rt, None).await;
    assert_eq!(result.source, Some(ResultSource::Cache));
    assert_eq!(result.media.unwrap().thumbnail, vec![1, 2, 3]);
    assert_eq!(renderer.calls.load(Ordering::Relaxed), 0);
    assert!(requests(&url).is_empty());
}

#[tokio::test]
async fn missing_handoff_handlers_skip_durable_storage_across_multiple_hops() {
    crate::handoff::register_handoff_fn(Box::new(|target, _, payload| Box::pin(async move {
        let mut rt = runtime(CacheStore::none());
        Arc::make_mut(&mut rt).renderer = Some(capability_renderer(target == "https://available.example"));
        if target == "https://proxy.example" {
            Arc::make_mut(&mut rt).handoff_tier2.url = Some("https://unavailable.example".into());
        }
        let (mut result, trace, mut after) = ThumbCook::<MockStream>::from_handoff(payload, rt).run().await;
        for task in after.drain() {
            task.await;
        }
        if let Some(ref mut media) = result.media {
            if !media.placeholder.is_empty() {
                media.thumbnail.clear();
            }
        }
        let wire = serde_json::to_vec(&HandoffResponse { result, trace }).unwrap();
        Ok(serde_json::from_slice(&wire).unwrap())
    })));
    for (target, hops) in [("https://unavailable.example", 1), ("https://proxy.example", 2)] {
        let url = register_path(&format!("missing-handoff-{hops}"), "image.svg",
            (0..=hops + 2).map(|_| vector_reply()).collect());
        let backend = memory();
        let mut rt = runtime(CacheStore::new(backend.clone(), 5));
        let config = Arc::make_mut(&mut rt);
        config.handoff_tier2.url = Some(target.into());
        config.renderer = Some(capability_renderer(false));
        let mut cook = ThumbCook::<MockStream>::new(&url, rt.clone());
        cook.default_pin_ttl = 600;
        let (result, _, mut after) = cook.run().await;
        for task in after.drain() {
            task.await;
        }
        assert_eq!(result.source, Some(ResultSource::Placeholder));
        let media = result.media.unwrap();
        assert_eq!(media.placeholder, "vector");
        assert!(!media.thumbnail.is_empty());
        assert!(!media.cache.is_empty());
        assert!(backend.get_entry(&url, 0).await.is_none());
        assert!(backend.get_pin(&url).await.is_none());
        assert_eq!(requests(&url).len(), hops + 1);
        assert_eq!(run(&url, rt, None).await.source, Some(ResultSource::Cache));
        assert_eq!(requests(&url).len(), hops + 1);

        let mut upgraded = runtime(CacheStore::new(backend.clone(), 5));
        let config = Arc::make_mut(&mut upgraded);
        config.handoff_tier2.url = Some("https://available.example".into());
        config.renderer = Some(capability_renderer(false));
        let rendered = run(&url, upgraded.clone(), None).await;
        assert_eq!(rendered.source, Some(ResultSource::Render));
        assert!(rendered.media.as_ref().unwrap().placeholder.is_empty());
        assert_eq!(backend.get_entry(&url, 0).await.unwrap().media.thumbnail, rendered.media.unwrap().thumbnail);
        assert_eq!(run(&url, upgraded, None).await.source, Some(ResultSource::Cache));
        assert_eq!(requests(&url).len(), hops + 3);
    }
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
async fn generated_and_cached_thumbnails_publish_resolvable_pins_before_returning() {
    for backend in [
        memory(),
        Arc::new(SqliteCacheBackend::open(":memory:").unwrap()),
    ] {
        let url = register(&format!("pin-generated-{}", backend.name()), vec![vector_reply()]);
        let measured = Arc::new(CountedPinBackend { backend: backend.clone(), calls: Default::default() });
        let mut rt = runtime(CacheStore::new(measured.clone(), 5));
        let config = Arc::make_mut(&mut rt);
        config.pin_ttl_secs = 90;
        config.renderer = Some(capability_renderer(true));
        let generated = run(&url, rt.clone(), None).await;
        let pin = generated.pin.clone().expect("generated thumbnail has a pin");
        let id = pin.strip_prefix("pin/").unwrap().strip_suffix(".jpeg").unwrap();
        assert_eq!(rt.cache.pin_thumbnail(id).await.unwrap().unwrap(), generated.media.as_ref().unwrap().thumbnail);
        let first_deadline = backend.get_pin_entry(&url).await.unwrap().pin_until;
        Arc::make_mut(&mut rt).pin_ttl_secs = 180;
        let before_calls = measured.calls.load(Ordering::Relaxed);
        let debounced = run(&url, rt.clone(), None).await;
        assert_eq!(measured.calls.load(Ordering::Relaxed), before_calls);
        assert_eq!(debounced.source, Some(ResultSource::Cache));
        assert_eq!(debounced.pin, Some(pin.clone()));
        assert_eq!(backend.get_pin_entry(&url).await.unwrap().pin_until, first_deadline);
        assert_eq!(requests(&url).len(), 1);

        let client = CacheHints::decode(&generated.media.as_ref().unwrap().cache).unwrap();
        let mut client_rt = runtime(CacheStore::backend_only(backend.clone()));
        Arc::make_mut(&mut client_rt).pin_ttl_secs = 180;
        let fresh = run(&url, client_rt, Some(client)).await;
        assert_eq!(fresh.source, Some(ResultSource::NotModified));
        assert_eq!(fresh.pin, Some(pin));
        assert!(fresh.media.unwrap().thumbnail.is_empty());
        assert!(backend.get_pin_entry(&url).await.unwrap().pin_until >= first_deadline + 89);

        Arc::make_mut(&mut rt).pin_ttl_secs = 0;
        let disabled = run(&url, rt, None).await;
        assert!(disabled.pin.is_none());
        assert!(serde_json::to_value(disabled).unwrap()["pin"].is_null());
    }
}

#[tokio::test]
async fn missing_handlers_publish_untracked_placeholder_pins_but_client_only_tokens_do_not() {
    let url = register_path("pin-missing-handler", "image.svg", vec![vector_reply()]);
    let backend = memory();
    let mut rt = runtime(CacheStore::new(backend.clone(), 5));
    Arc::make_mut(&mut rt).pin_ttl_secs = 60;
    let first = run(&url, rt.clone(), None).await;
    assert_eq!(first.source, Some(ResultSource::Placeholder));
    let id = backend.pin_candidate(FileKind::Vector, &url, 0).unwrap();
    assert_eq!(first.pin, Some(format!("pin/{id}.jpeg")));
    let debounced = run(&url, rt.clone(), None).await;
    assert_eq!(debounced.pin, first.pin);
    Arc::make_mut(&mut rt).pin_ttl_secs = 0;
    assert!(run(&url, rt, None).await.pin.is_none());
    assert!(backend.get_pin(&url).await.is_none());
    assert!(backend.get_entry(&url, 0).await.is_none());

    let url = register("pin-client-only", vec![]);
    let mut rt = runtime(CacheStore::backend_only(backend));
    Arc::make_mut(&mut rt).pin_ttl_secs = 60;
    assert!(run(&url, rt, Some(hints(None, true))).await.pin.is_none());
}

#[tokio::test]
async fn a_handler_upgrade_activates_the_previously_untracked_placeholder_pin() {
    let url = register_path("pin-placeholder-no-cache", "image.svg", vec![vector_reply()]);
    let mut rt = runtime(CacheStore::none());
    Arc::make_mut(&mut rt).pin_ttl_secs = 60;
    let fallback = run(&url, rt, None).await;
    assert!(fallback.pin.is_none());

    let url = register_path("pin-placeholder-upgrade", "image.svg", vec![vector_reply(), vector_reply()]);
    let backend = memory();
    let mut rt = runtime(CacheStore::backend_only(backend.clone()));
    Arc::make_mut(&mut rt).pin_ttl_secs = 60;
    let fallback = run(&url, rt.clone(), None).await;
    let expected = backend.pin_candidate(FileKind::Vector, &url, 0).unwrap();
    assert_eq!(fallback.pin, Some(format!("pin/{expected}.jpeg")));
    assert!(backend.get_entry(&url, 0).await.is_none());
    assert!(backend.get_pin_entry(&url).await.is_none());
    Arc::make_mut(&mut rt).renderer = Some(capability_renderer(true));
    let rendered = run(&url, rt.clone(), None).await;
    let pin = rendered.pin.as_deref().unwrap();
    assert_eq!(Some(pin), fallback.pin.as_deref());
    let id = pin.strip_prefix("pin/").unwrap().strip_suffix(".jpeg").unwrap();
    assert_eq!(rt.cache.pin_thumbnail(id).await.unwrap().unwrap(), rendered.media.unwrap().thumbnail);
}

#[tokio::test]
async fn deriving_a_placeholder_pin_does_not_read_entries_or_write_pin_bookkeeping() {
    let measured = Arc::new(CountedPinBackend { backend: memory(), calls: Default::default() });
    let store = CacheStore::backend_only(measured.clone());
    let result = ThumbResult {
        status: ResultStatus::Success,
        source: Some(ResultSource::Placeholder),
        media: Some(ThumbMedia { kind: FileKind::Image, placeholder: "image".into(), ..Default::default() }),
        ..Default::default()
    };
    let pin = store.pin_result("source", &result, 60).await.unwrap().unwrap();
    // Only candidate derivation is called; every backend lookup/write is counted.
    assert_eq!(measured.calls.load(Ordering::Relaxed), 1);
    let expected = measured.backend.pin_candidate(FileKind::Image, "source", 0).unwrap();
    assert_eq!(pin, format!("pin/{expected}.jpeg"));
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
