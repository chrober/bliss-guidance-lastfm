use bliss_guidance_lastfm::freshness::{canonical_artist_payload_hash, expires_at, FreshnessClass};
use bliss_guidance_lastfm::lastfm_api::{ArtistLimit, LastFmClient, ResponseStatus, Validators};
use bliss_guidance_lastfm::store::{
    ArtistEdgeWrite, ArtistObservationWrite, GraphStore, ObservationStatus, ObservationValidation,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

fn database_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "bliss-guidance-lastfm-http-{label}-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn serve_once(response: &'static str) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = [0_u8; 16_384];
        let size = stream.read(&mut bytes).unwrap();
        let request = String::from_utf8_lossy(&bytes[..size]).into_owned();
        stream.write_all(response.as_bytes()).unwrap();
        request
    });
    (format!("http://{address}/2.0/"), handle)
}

#[test]
fn failures_expose_safe_http_and_parse_context_without_credentials() {
    for (response, expected) in [
        ("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "HTTP status 429; retryable=yes; retry-after=60s"),
        ("HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json", "stage=json_decode; retryable=no"),
        ("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}", "stage=result_shape; retryable=no"),
        ("HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"error\":10}", "Last.fm error code 10; retryable=no"),
    ] {
        let (endpoint, server) = serve_once(response);
        let client = LastFmClient::new(&endpoint, "secret-fixture-key", Duration::from_secs(2)).unwrap();
        let error = client.similar_artists("Source", None, ArtistLimit::TwentyFive, None).unwrap_err();
        let detail = error.diagnostic();
        assert!(detail.contains(expected), "{detail}");
        assert!(!detail.contains("secret-fixture-key"));
        server.join().unwrap();
    }
}

#[test]
fn track_cache_does_not_grow_past_its_remaining_budget() {
    use bliss_guidance_lastfm::legacy_cache::{cache_tracks_with_budget, cached_tracks};
    let path = database_path("track-budget").with_extension("json");
    assert!(!cache_tracks_with_budget(&path, "test", 100, &[], 1).unwrap());
    assert!(!path.exists());
    assert!(cache_tracks_with_budget(&path, "test", 100, &[], 1000).unwrap());
    let before = std::fs::read(&path).unwrap();
    assert!(!cache_tracks_with_budget(&path, "another", 100, &[], before.len() as u64).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(cached_tracks(&path, "test", 101).unwrap().is_some());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("lock"));
}

#[test]
fn artist_response_captures_limit_validators_and_freshness_headers() {
    let body =
        r#"{"similarartists":{"artist":[{"name":"Target","mbid":"target-mbid","match":"0.75"}]}}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: public, max-age=7200\r\nETag: \"artist-v1\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let leaked: &'static str = Box::leak(response.into_boxed_str());
    let (endpoint, server) = serve_once(leaked);
    let client = LastFmClient::new(&endpoint, "test-key", Duration::from_secs(2)).unwrap();

    let result = client
        .similar_artists("Source", Some("source-mbid"), ArtistLimit::Fifty, None)
        .unwrap();

    assert_eq!(result.status, ResponseStatus::Modified);
    assert_eq!(result.requested_limit, 50);
    assert_eq!(result.actual_result_count, 1);
    assert_eq!(result.results[0].name, "Target");
    assert_eq!(
        result.headers.cache_control.as_deref(),
        Some("public, max-age=7200")
    );
    assert_eq!(result.headers.etag.as_deref(), Some("\"artist-v1\""));
    assert_eq!(
        result.headers.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    let request = server.join().unwrap();
    assert!(request.contains("limit=50"));
}

#[test]
fn artist_response_accepts_custom_limits_and_omits_limit_when_unlimited() {
    let body = r#"{"similarartists":{"artist":[]}}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let leaked: &'static str = Box::leak(response.clone().into_boxed_str());
    let (endpoint, server) = serve_once(leaked);
    let client = LastFmClient::new(&endpoint, "test-key", Duration::from_secs(2)).unwrap();
    let result = client
        .similar_artists("Source", None, ArtistLimit::Custom(37), None)
        .unwrap();
    assert_eq!(result.requested_limit, 37);
    assert!(server.join().unwrap().contains("limit=37"));

    let leaked: &'static str = Box::leak(response.into_boxed_str());
    let (endpoint, server) = serve_once(leaked);
    let client = LastFmClient::new(&endpoint, "test-key", Duration::from_secs(2)).unwrap();
    let result = client
        .similar_artists("Source", None, ArtistLimit::Unlimited, None)
        .unwrap();
    assert_eq!(result.requested_limit, 0);
    assert!(!server.join().unwrap().contains("limit="));
}

#[test]
fn conditional_request_accepts_not_modified_without_parsing_a_body() {
    let (endpoint, server) = serve_once(
        "HTTP/1.1 304 Not Modified\r\nETag: \"artist-v1\"\r\nCache-Control: max-age=3600\r\nConnection: close\r\n\r\n",
    );
    let client = LastFmClient::new(&endpoint, "test-key", Duration::from_secs(2)).unwrap();
    let result = client
        .similar_artists(
            "Source",
            None,
            ArtistLimit::TwentyFive,
            Some(&Validators {
                etag: Some("\"artist-v1\"".to_owned()),
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
            }),
        )
        .unwrap();
    assert_eq!(result.status, ResponseStatus::NotModified);
    assert!(result.results.is_empty());
    let request = server.join().unwrap();
    assert!(request.contains("If-None-Match: \"artist-v1\""));
    assert!(request.contains("If-Modified-Since: Wed, 21 Oct 2015 07:28:00 GMT"));
}

#[test]
fn freshness_prefers_http_metadata_and_has_bounded_fallbacks() {
    let now = 1_700_000_000_i64;
    assert_eq!(
        expires_at(
            now,
            Some("public, max-age=120"),
            None,
            FreshnessClass::ArtistPositive
        ),
        now + 120
    );
    assert_eq!(
        expires_at(now, None, None, FreshnessClass::ArtistPositive),
        now + 30 * 86_400
    );
    assert_eq!(
        expires_at(now, None, None, FreshnessClass::ArtistEmpty),
        now + 7 * 86_400
    );
    assert_eq!(
        expires_at(now, None, None, FreshnessClass::Track),
        now + 86_400
    );
}

#[test]
fn configured_refresh_targets_use_last_validation_and_override_automatic_expiry() {
    use bliss_guidance_lastfm::freshness::artist_observation_is_fresh;
    let now = 1_700_000_000;
    let validated = now - 10 * 86_400;
    assert!(artist_observation_is_fresh(
        now,
        validated,
        now + 86_400,
        None
    ));
    assert!(!artist_observation_is_fresh(now, validated, now - 1, None));
    assert!(!artist_observation_is_fresh(
        now,
        validated,
        now + 86_400,
        Some(7)
    ));
    assert!(artist_observation_is_fresh(
        now,
        validated,
        now - 1,
        Some(30)
    ));
    assert!(!artist_observation_is_fresh(
        now,
        validated,
        now + 86_400,
        Some(10)
    ));
}

#[test]
fn ordered_payload_hash_changes_with_rank_identity_or_score() {
    let base = vec![("target-mbid", "Target", 1_u32, Some(0.75_f64))];
    let same = canonical_artist_payload_hash("artist.getSimilar", "source-mbid", &base);
    assert_eq!(
        same,
        canonical_artist_payload_hash("artist.getSimilar", "source-mbid", &base)
    );
    assert_ne!(
        same,
        canonical_artist_payload_hash(
            "artist.getSimilar",
            "source-mbid",
            &[("target-mbid", "Target", 2, Some(0.75))]
        )
    );
    assert_ne!(
        same,
        canonical_artist_payload_hash(
            "artist.getSimilar",
            "source-mbid",
            &[("target-mbid", "Target", 1, Some(0.70))]
        )
    );
}

#[test]
fn throttling_and_transport_failures_are_retryable() {
    let body = r#"{"error":29,"message":"Rate limit exceeded"}"#;
    let response = format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let leaked: &'static str = Box::leak(response.into_boxed_str());
    let (endpoint, server) = serve_once(leaked);
    let client = LastFmClient::new(&endpoint, "test-key", Duration::from_secs(2)).unwrap();
    let error = client
        .similar_artists("Source", None, ArtistLimit::TwentyFive, None)
        .unwrap_err();
    assert!(error.is_retryable());
    server.join().unwrap();

    let body = r#"{"error":29,"message":"Rate limit exceeded"}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let leaked: &'static str = Box::leak(response.into_boxed_str());
    let (endpoint, server) = serve_once(leaked);
    let client = LastFmClient::new(&endpoint, "test-key", Duration::from_secs(2)).unwrap();
    let error = client
        .similar_artists("Source", None, ArtistLimit::TwentyFive, None)
        .unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(error.message(), "direct Last.fm request was rate limited");
    server.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/2.0/", listener.local_addr().unwrap());
    drop(listener);
    let client = LastFmClient::new(&endpoint, "test-key", Duration::from_millis(100)).unwrap();
    let error = client
        .similar_artists("Source", None, ArtistLimit::TwentyFive, None)
        .unwrap_err();
    assert!(error.is_retryable());
}

#[test]
fn revalidation_preserves_edges_and_change_time_for_304_and_unchanged_200() {
    let path = database_path("revalidate");
    let mut store = GraphStore::open(&path).unwrap();
    let source = store.upsert_artist(None, "Source", 100).unwrap();
    let target = store.upsert_artist(None, "Target", 100).unwrap();
    store
        .replace_artist_observation(
            source,
            &ArtistObservationWrite {
                fetched_at: 100,
                validated_at: 100,
                changed_at: 100,
                expires_at: 200,
                etag: Some("old".to_owned()),
                last_modified: None,
                payload_hash: "same-hash".to_owned(),
                acquisition_source: "api_key".to_owned(),
                requested_limit: 25,
                actual_result_count: 1,
                result_status: ObservationStatus::Positive,
            },
            &[ArtistEdgeWrite {
                target_artist_id: target,
                rank: 1,
                match_score: Some(0.8),
            }],
        )
        .unwrap();
    let before_revalidation_bytes = store.retained_lastfm_bytes().unwrap();

    store
        .revalidate_artist_observation(
            source,
            &ObservationValidation {
                fetched_at: None,
                validated_at: 300,
                expires_at: 600,
                etag: Some("new".to_owned()),
                last_modified: Some("later".to_owned()),
                requested_limit: None,
                actual_result_count: None,
            },
        )
        .unwrap();
    let after_304 = store.artist_observation(source).unwrap().unwrap();
    assert_eq!(after_304.fetched_at, 100);
    assert_eq!(after_304.validated_at, 300);
    assert_eq!(after_304.changed_at, 100);
    assert_eq!(after_304.edges.len(), 1);
    let after_304_bytes = store.retained_lastfm_bytes().unwrap();
    assert!(after_304_bytes > before_revalidation_bytes);

    store
        .revalidate_artist_observation(
            source,
            &ObservationValidation {
                fetched_at: Some(400),
                validated_at: 400,
                expires_at: 800,
                etag: None,
                last_modified: None,
                requested_limit: Some(50),
                actual_result_count: Some(1),
            },
        )
        .unwrap();
    let unchanged_200 = store.artist_observation(source).unwrap().unwrap();
    assert_eq!(unchanged_200.fetched_at, 400);
    assert_eq!(unchanged_200.changed_at, 100);
    assert_eq!(unchanged_200.requested_limit, 50);
    assert_eq!(unchanged_200.edges.len(), 1);
    assert_eq!(store.retained_lastfm_bytes().unwrap(), after_304_bytes);

    drop(store);
    let _ = std::fs::remove_file(path);
}
