use bliss_guidance_lastfm::direct::{DirectOptions, DirectPrepared, API_KEY_ENVIRONMENT};
use bliss_playlist_guidance_spi::{Anchor, Candidate, GuidanceScope, ScoreContext};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static API_KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn store_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "lastfm-read-through-{}-{}.sqlite",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn options(endpoint: &str, store: &std::path::Path, policy: &str) -> DirectOptions {
    let legacy_cache = store.with_extension("json");
    DirectOptions::from_prepare_options(&serde_json::json!({
        "acquisition_mode": "direct",
        "api_endpoint": endpoint,
        "store_path": store,
        "legacy_cache_path": legacy_cache,
        "retrieval_policy": policy,
        "artist_result_limit": 25,
        "storage_scope": "all_returned_artists",
        "request_deadline_ms": 2000,
        "max_concurrent_requests": 2,
        "diagnostic_level": "debug"
    }))
    .unwrap()
}

fn anchor() -> Anchor {
    Anchor {
        anchor_id: "seed".to_owned(),
        track: Candidate {
            candidate_id: "seed".to_owned(),
            lms_urlmd5: None,
            database_file: None,
            title: Some("Seed Song".to_owned()),
            artist: Some("Seed Artist".to_owned()),
            album: None,
            recording_mbid: Some("seed-recording".to_owned()),
            artist_mbids: vec!["seed-artist".to_owned()],
        },
    }
}

fn candidate() -> Candidate {
    Candidate {
        candidate_id: "candidate".to_owned(),
        lms_urlmd5: None,
        database_file: None,
        title: Some("Related Song".to_owned()),
        artist: Some("Related Artist".to_owned()),
        album: None,
        recording_mbid: Some("related-recording".to_owned()),
        artist_mbids: vec!["related-artist".to_owned()],
    }
}

#[test]
fn local_only_refreshes_library_before_persisting_artist_results() {
    let _guard = API_KEY_LOCK.lock().unwrap();
    std::env::set_var(API_KEY_ENVIRONMENT, "test-key");
    let graph = store_path();
    let library = graph.with_extension("library.sqlite");
    let db = rusqlite::Connection::open(&library).unwrap();
    db.execute_batch("CREATE TABLE contributors(id INTEGER PRIMARY KEY,name TEXT,musicbrainz_id TEXT); CREATE TABLE tracks(id INTEGER PRIMARY KEY,remote INTEGER); CREATE TABLE contributor_track(contributor INTEGER,track INTEGER); INSERT INTO tracks VALUES(1,0); INSERT INTO contributors VALUES(1,'Seed Artist','seed-artist'),(2,'Related Artist','related-artist'); INSERT INTO contributor_track VALUES(1,1),(2,1);").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            let body = if request.contains("track.getSimilar") {
                r#"{"similartracks":{"track":[]}}"#
            } else {
                r#"{"similarartists":{"artist":[{"name":"Related Artist","mbid":"related-artist","match":"0.8"}]}}"#
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=3600\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    let mut opts = options(&endpoint, &graph, "online_with_cache");
    opts.storage_scope = bliss_guidance_lastfm::direct::StorageScope::LocalArtistsOnly;
    opts.library_db_path = Some(library.to_string_lossy().into_owned());
    let (_, diagnostics) = DirectPrepared::prepare(&opts, &[anchor()]).unwrap();
    server.join().unwrap();
    assert_eq!(diagnostics.request_count, 2);
    let store = bliss_guidance_lastfm::store::GraphStore::open(&graph).unwrap();
    let id = store
        .find_artist(&bliss_guidance_lastfm::store::ArtistIdentity {
            name: "Seed Artist".into(),
            mbid: Some("seed-artist".into()),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        store.artist_observation(id).unwrap().unwrap().edges.len(),
        1
    );
}

#[test]
fn online_fill_is_reused_by_a_new_offline_only_session() {
    let _guard = API_KEY_LOCK.lock().unwrap();
    std::env::set_var(API_KEY_ENVIRONMENT, "test-key");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            let body = if request.contains("track.getSimilar") {
                r#"{"similartracks":{"track":[{"name":"Related Song","mbid":"related-recording","match":"0.9","artist":{"name":"Related Artist","mbid":"related-artist"}}]}}"#
            } else {
                r#"{"similarartists":{"artist":[{"name":"Related Artist","mbid":"related-artist","match":"0.8"}]}}"#
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: max-age=3600\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });
    let store = store_path();
    let (online, online_diagnostics) = DirectPrepared::prepare(
        &options(&endpoint, &store, "online_with_cache"),
        &[anchor()],
    )
    .unwrap();
    server.join().unwrap();
    assert_eq!(online_diagnostics.request_count, 2);

    let unreachable = "http://127.0.0.1:9";
    let (offline, offline_diagnostics) =
        DirectPrepared::prepare(&options(unreachable, &store, "offline_only"), &[anchor()])
            .unwrap();
    assert_eq!(offline_diagnostics.request_count, 0);
    let context = ScoreContext {
        scope: GuidanceScope::Edge,
        left_anchor_id: Some("seed".to_owned()),
        right_anchor_id: None,
        context_track_ids: vec![],
    };
    assert_eq!(
        online.signals(&context, &[candidate()]),
        offline.signals(&context, &[candidate()])
    );
    assert_eq!(offline.signals(&context, &[candidate()]).len(), 2);
    let graph = bliss_guidance_lastfm::store::GraphStore::open(&store).unwrap();
    assert!(!graph.has_table("tracks").unwrap());

    std::env::remove_var(API_KEY_ENVIRONMENT);
    drop(online);
    drop(offline);
    thread::sleep(Duration::from_millis(10));
    let _ = std::fs::remove_file(&store);
    let _ = std::fs::remove_file(store.with_extension("json"));
}

#[test]
fn offline_only_missing_data_is_neutral_and_makes_no_request() {
    let _guard = API_KEY_LOCK.lock().unwrap();
    std::env::set_var(API_KEY_ENVIRONMENT, "test-key");
    let store = store_path();
    let (prepared, diagnostics) = DirectPrepared::prepare(
        &options("http://127.0.0.1:9", &store, "offline_only"),
        &[anchor()],
    )
    .unwrap();
    assert_eq!(diagnostics.request_count, 0);
    assert_eq!(diagnostics.state.as_deref(), Some("missing"));
    assert!(prepared
        .signals(
            &ScoreContext {
                scope: GuidanceScope::Edge,
                left_anchor_id: Some("seed".to_owned()),
                right_anchor_id: None,
                context_track_ids: vec![],
            },
            &[candidate()]
        )
        .is_empty());
    std::env::remove_var(API_KEY_ENVIRONMENT);
    let _ = std::fs::remove_file(&store);
    let _ = std::fs::remove_file(store.with_extension("json"));
}

#[test]
fn custom_positive_target_revalidates_during_guidance_preparation() {
    use bliss_guidance_lastfm::store::{ArtistObservationWrite, GraphStore, ObservationStatus};
    let _guard = API_KEY_LOCK.lock().unwrap();
    std::env::set_var(API_KEY_ENVIRONMENT, "test-key");
    let store = store_path();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut graph = GraphStore::open(&store).unwrap();
    let id = graph
        .upsert_artist(Some("seed-artist"), "Seed Artist", now)
        .unwrap();
    graph
        .replace_artist_observation(
            id,
            &ArtistObservationWrite {
                fetched_at: now - 40 * 86400,
                validated_at: now - 40 * 86400,
                changed_at: now - 40 * 86400,
                expires_at: now + 30 * 86400,
                etag: Some("\"seed-etag\"".to_owned()),
                last_modified: None,
                payload_hash: "fixture".to_owned(),
                acquisition_source: "api_key".to_owned(),
                requested_limit: 25,
                actual_result_count: 1,
                result_status: ObservationStatus::Positive,
            },
            &[],
        )
        .unwrap();
    drop(graph);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0_u8; 8192];
        let n = stream.read(&mut buffer).unwrap();
        let request = String::from_utf8_lossy(&buffer[..n]).to_ascii_lowercase();
        assert!(request.contains("if-none-match: \"seed-etag\""));
        stream.write_all(b"HTTP/1.1 304 Not Modified\r\nCache-Control: max-age=3600\r\nConnection: close\r\n\r\n").unwrap();
    });
    let mut settings = options(&endpoint, &store, "online_with_cache");
    settings.artist_refresh_days = Some(30);
    let mut seed = anchor();
    seed.track.title = None;
    seed.track.recording_mbid = None;
    let (_, diagnostics) = DirectPrepared::prepare(&settings, &[seed]).unwrap();
    server.join().unwrap();
    assert_eq!(diagnostics.request_count, 1);
    let graph = GraphStore::open(&store).unwrap();
    assert!(graph.artist_observation(id).unwrap().unwrap().validated_at >= now);
    drop(graph);
    std::env::remove_var(API_KEY_ENVIRONMENT);
    let _ = std::fs::remove_file(&store);
    let _ = std::fs::remove_file(store.with_extension("json"));
}
