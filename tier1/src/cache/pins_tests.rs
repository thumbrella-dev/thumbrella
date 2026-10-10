use super::pins::*;
use super::*;
use crate::media::FileKind;

fn result(bytes: Vec<u8>) -> ThumbResult {
    ThumbResult {
        status: ResultStatus::Success,
        source: Some(ResultSource::Render),
        media: Some(ThumbMedia {
            thumbnail: bytes,
            kind: FileKind::Image,
            cache: CacheHints::expiring_in(60).encode(60),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn backends() -> Vec<Arc<dyn CacheBackend>> {
    vec![
        Arc::new(memory::MemoryCacheBackend::with_max_entries(100).unwrap()),
        Arc::new(sqlite::SqliteCacheBackend::open(":memory:").unwrap()),
        Arc::new(chain::ChainCacheBackend::new(vec![
            Arc::new(memory::MemoryCacheBackend::with_max_entries(100).unwrap()),
            Arc::new(sqlite::SqliteCacheBackend::open(":memory:").unwrap()),
        ])),
    ]
}

#[test]
fn pin_identifiers_have_unique_kind_letters_and_twelve_url_safe_hash_characters() {
    let kinds = [
        FileKind::Image,
        FileKind::Video,
        FileKind::Audio,
        FileKind::Vector,
        FileKind::Document,
        FileKind::Geometry,
        FileKind::Archive,
        FileKind::Text,
        FileKind::Binary,
        FileKind::Unknown,
    ];
    let secret = PinSecret::from_bytes(vec![0x0b; 32]).unwrap();
    let other = PinSecret::from_bytes(vec![0x0c; 32]).unwrap();
    assert_eq!(
        secret.candidate(FileKind::Image, "https://example.com/image.jpg", 0),
        "ilWcYW3cK572k"
    );
    let mut letters = std::collections::HashSet::new();
    for kind in kinds {
        assert!(letters.insert(kind_letter(kind)));
        let id = secret.candidate(kind, "https://example.com/image.jpg", 0);
        assert!(valid_pin(&id));
        assert_eq!(pin_kind(&id), kind);
        assert_eq!(id, secret.candidate(kind, "https://example.com/image.jpg", 0));
        assert_ne!(id, other.candidate(kind, "https://example.com/image.jpg", 0));
        assert_ne!(id, secret.candidate(kind, "https://example.com/image.jpg", 1));
    }
    for invalid in [
        "",
        "i",
        "x123456789012",
        "i12345678901/",
        "i12345678901=",
        "i1234567890123",
        "i💡💡💡",
    ] {
        assert!(!valid_pin(invalid));
    }
}

#[test]
fn memory_backends_have_independent_secrets_and_clones_share_the_same_identity() {
    let first = Arc::new(memory::MemoryCacheBackend::with_max_entries(100).unwrap());
    let second = memory::MemoryCacheBackend::with_max_entries(100).unwrap();
    let shared = first.clone();
    let id = first.pin_candidate(FileKind::Image, "source", 0).unwrap();
    assert_eq!(id, shared.pin_candidate(FileKind::Image, "source", 0).unwrap());
    assert_ne!(id, second.pin_candidate(FileKind::Image, "source", 0).unwrap());
    for length in [0, 31, 33] {
        assert!(PinSecret::from_bytes(vec![0; length]).is_err());
    }
}

#[tokio::test]
async fn collision_exhaustion_is_an_explicit_error_and_disabled_issuance_is_a_noop() {
    let backend = memory::MemoryCacheBackend::with_max_entries(100).unwrap();
    let now = unix_now_secs();
    for key in ["first", "second"] {
        backend.put(key.into(), result(vec![1]).media.unwrap(), 0, now + 60, 60).await;
    }
    for attempt in 0..256 {
        let id = backend.pin_candidate(FileKind::Image, "second", attempt).unwrap();
        assert_eq!(backend.claim_pin(&id, "first", 60).await.unwrap(), PinClaim::Claimed);
    }
    assert!(
        backend
            .issue_pin(FileKind::Image, "second", 60)
            .await
            .unwrap_err()
            .contains("collision limit")
    );
    assert!(backend.issue_pin(FileKind::Image, "second", 0).await.unwrap().is_none());
    assert!(backend.issue_pin(FileKind::Image, "absent", 60).await.unwrap().is_none());
}

#[tokio::test]
async fn pins_follow_updates_outlive_cache_retention_and_reads_do_not_refresh() {
    for backend in backends() {
        let now = unix_now_secs();
        let store = CacheStore::backend_only(backend.clone());
        let first = result(vec![1, 2, 3]);
        backend.put("source".into(), first.media.clone().unwrap(), 0, now + 1, 30).await;
        let pin = store.pin_result("source", &first, 90).await.unwrap().unwrap();
        let id = pin.strip_prefix("pin/").unwrap().strip_suffix(".jpeg").unwrap();
        let before = backend.get_pin_entry("source").await.unwrap();
        assert!(before.pin_until >= now + 90);
        assert_eq!(store.pin_thumbnail(id).await.unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(backend.get_pin_entry("source").await.unwrap().pin_until, before.pin_until);

        let second = result(vec![4, 5]);
        backend.put("source".into(), second.media.clone().unwrap(), 0, 0, 30).await;
        assert!(backend.get_entry("source", 0).await.is_none());
        assert_eq!(store.pin_result("source", &second, 90).await.unwrap(), Some(pin.clone()));
        assert_eq!(store.pin_thumbnail(id).await.unwrap(), Some(vec![4, 5]));
        assert!(backend.get_pin_entry("source").await.unwrap().pin_until >= before.pin_until);
    }
}

#[tokio::test]
async fn collisions_are_retried_and_never_replace_the_existing_thumbnail() {
    for backend in backends() {
        let first = result(vec![1]);
        let second = result(vec![2]);
        let now = unix_now_secs();
        backend.put("first".into(), first.media.clone().unwrap(), 0, now + 60, 60).await;
        backend
            .put("second".into(), second.media.clone().unwrap(), 0, now + 60, 60)
            .await;
        let collision = backend.pin_candidate(FileKind::Image, "second", 0).unwrap();
        assert_eq!(backend.claim_pin(&collision, "first", 60).await.unwrap(), PinClaim::Claimed);
        assert_eq!(backend.claim_pin(&collision, "second", 60).await.unwrap(), PinClaim::Conflict);
        let store = CacheStore::backend_only(backend.clone());
        let mut placeholder = second.clone();
        placeholder.media.as_mut().unwrap().placeholder = "image".into();
        assert_eq!(
            store.pin_result("second", &placeholder, 60).await.unwrap(),
            Some(format!("pin/{collision}.jpeg"))
        );
        assert_eq!(store.pin_thumbnail(&collision).await.unwrap(), Some(vec![1]));
        let pin = store.pin_result("second", &second, 60).await.unwrap().unwrap();
        let expected = backend.pin_candidate(FileKind::Image, "second", 1).unwrap();
        assert_eq!(pin, format!("pin/{expected}.jpeg"));
        assert_eq!(store.pin_thumbnail(&collision).await.unwrap(), Some(vec![1]));
        assert_eq!(store.pin_thumbnail(&expected).await.unwrap(), Some(vec![2]));
    }
}

#[tokio::test]
async fn concurrent_claims_have_exactly_one_owner() {
    for backend in backends().into_iter().take(2) {
        let now = unix_now_secs();
        backend
            .put("first".into(), result(vec![1]).media.unwrap(), 0, now + 60, 60)
            .await;
        backend
            .put("second".into(), result(vec![2]).media.unwrap(), 0, now + 60, 60)
            .await;
        let first = {
            let backend = backend.clone();
            tokio::spawn(async move { backend.claim_pin("i123456789012", "first", 60).await.unwrap() })
        };
        let second = {
            let backend = backend.clone();
            tokio::spawn(async move { backend.claim_pin("i123456789012", "second", 60).await.unwrap() })
        };
        let outcomes = [first.await.unwrap(), second.await.unwrap()];
        assert_eq!(outcomes.iter().filter(|outcome| **outcome == PinClaim::Claimed).count(), 1);
        assert_eq!(outcomes.iter().filter(|outcome| **outcome == PinClaim::Conflict).count(), 1);
    }
}

#[tokio::test]
async fn expired_pins_are_misses_even_while_the_cache_entry_is_live() {
    let backends = backends();
    for backend in &backends {
        backend
            .put("source".into(), result(vec![1]).media.unwrap(), 0, unix_now_secs() + 60, 1)
            .await;
        assert_eq!(
            backend.claim_pin("i123456789012", "source", 1).await.unwrap(),
            PinClaim::Claimed
        );
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    for backend in backends {
        assert!(backend.pin_thumbnail("i123456789012").await.unwrap().is_none());
        assert!(backend.get_entry("source", 0).await.is_some());
    }
}

#[tokio::test]
async fn absent_entries_and_disabled_pins_return_none_while_placeholders_get_untracked_pins() {
    for backend in backends() {
        let store = CacheStore::backend_only(backend.clone());
        let mut result = result(vec![1]);
        assert_eq!(
            backend.claim_pin("i123456789012", "absent", 60).await.unwrap(),
            PinClaim::Missing
        );
        let fresh = ThumbResult {
            source: Some(ResultSource::NotModified),
            media: Some(ThumbMedia {
                kind: FileKind::Image,
                ..Default::default()
            }),
            ..result.clone()
        };
        assert!(store.pin_result("absent", &fresh, 60).await.unwrap().is_none());
        backend
            .put("source".into(), result.media.clone().unwrap(), 0, unix_now_secs() + 60, 0)
            .await;
        assert!(store.pin_result("source", &result, 0).await.unwrap().is_none());
        result.media.as_mut().unwrap().placeholder = "image".into();
        let before = backend.get_entry("source", 0).await.unwrap();
        let id = backend.pin_candidate(FileKind::Image, "source", 0).unwrap();
        assert_eq!(
            store.pin_result("source", &result, 60).await.unwrap(),
            Some(format!("pin/{id}.jpeg"))
        );
        assert!(store.pin_thumbnail(&id).await.unwrap().is_none());
        let after = backend.get_entry("source", 0).await.unwrap();
        assert_eq!(after.pin_until, before.pin_until);
        assert_eq!(after.cache_until, before.cache_until);
    }
}

#[tokio::test]
async fn placeholder_pins_match_future_rendered_pins_without_storing_placeholder_entries() {
    for backend in backends() {
        let store = CacheStore::backend_only(backend.clone());
        for kind in [
            FileKind::Image,
            FileKind::Video,
            FileKind::Audio,
            FileKind::Vector,
            FileKind::Document,
            FileKind::Geometry,
            FileKind::Archive,
            FileKind::Text,
            FileKind::Binary,
            FileKind::Unknown,
        ] {
            let mut result = result(vec![1]);
            let media = result.media.as_mut().unwrap();
            media.kind = kind;
            media.placeholder = "placeholder".into();
            let url = store.pin_result("source", &result, 60).await.unwrap().unwrap();
            let id = url.strip_prefix("pin/").unwrap().strip_suffix(".jpeg").unwrap();
            assert!(valid_pin(id));
            assert_eq!(pin_kind(id), kind);
            assert!(store.pin_thumbnail(id).await.unwrap().is_none());
            assert!(backend.get_entry("source", 0).await.is_none());
            assert!(backend.get_pin_entry("source").await.is_none());
            assert_eq!(id, backend.pin_candidate(kind, "source", 0).unwrap());
            assert!(store.pin_result("source", &result, 0).await.unwrap().is_none());
            let mut failed = result.clone();
            failed.status = ResultStatus::Failed;
            assert!(store.pin_result("source", &failed, 60).await.unwrap().is_none());
        }
        let mut placeholder = result(vec![1]);
        placeholder.media.as_mut().unwrap().placeholder = "image".into();
        let first = store.pin_result("source", &placeholder, 60).await.unwrap().unwrap();
        let rendered = result(vec![2]);
        backend
            .put("source".into(), rendered.media.clone().unwrap(), 0, unix_now_secs() + 60, 0)
            .await;
        assert_eq!(store.pin_result("source", &rendered, 60).await.unwrap(), Some(first.clone()));
        let id = first.strip_prefix("pin/").unwrap().strip_suffix(".jpeg").unwrap();
        assert_eq!(store.pin_thumbnail(id).await.unwrap(), Some(vec![2]));
    }
}

#[tokio::test]
async fn chain_pins_restore_a_missing_cold_entry_and_survive_a_new_memory_front() {
    let hot = Arc::new(memory::MemoryCacheBackend::with_max_entries(100).unwrap());
    let cold = Arc::new(sqlite::SqliteCacheBackend::open(":memory:").unwrap());
    let result = result(vec![1, 2]);
    hot.put("source".into(), result.media.clone().unwrap(), 0, unix_now_secs() + 60, 0)
        .await;
    let chain = Arc::new(chain::ChainCacheBackend::new(vec![hot, cold.clone()]));
    let store = CacheStore::backend_only(chain);
    let pin = store.pin_result("source", &result, 60).await.unwrap().unwrap();
    let id = pin.strip_prefix("pin/").unwrap().strip_suffix(".jpeg").unwrap();
    let restarted = CacheStore::backend_only(Arc::new(chain::ChainCacheBackend::new(vec![
        Arc::new(memory::MemoryCacheBackend::with_max_entries(100).unwrap()),
        cold.clone(),
    ])));
    assert_eq!(restarted.pin_thumbnail(id).await.unwrap(), Some(vec![1, 2]));
    assert_eq!(restarted.pin_result("source", &result, 60).await.unwrap(), Some(pin.clone()));
    assert_eq!(id, cold.pin_candidate(FileKind::Image, "source", 0).unwrap());
}
