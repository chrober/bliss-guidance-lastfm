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
fn offline_preferred_fetches_missing_artist_for_current_job_and_persists_it() {
    use bliss_playlist_guidance_spi::{GuidanceRequest, GuidanceResponse, SPI_VERSION};
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let limit = Instant::now() + Duration::from_secs(8);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0_u8; 8192];
                    let _ = stream.read(&mut request).unwrap();
                    thread::sleep(Duration::from_millis(150));
                    let body = r#"{"similarartists":{"artist":[{"name":"Related Artist","mbid":"related-artist","match":"0.8"}]}}"#;
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=3600\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                    return true;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < limit =>
                {
                    thread::sleep(Duration::from_millis(20))
                }
                _ => return false,
            }
        }
    });
    let store = store_path();
    let mut seed = anchor();
    seed.track.title = None;
    let mut child = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .env(API_KEY_ENVIRONMENT, "test-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let request = GuidanceRequest::Prepare {
        spi_version: SPI_VERSION,
        job_id: "offline-fixture".into(),
        options: serde_json::json!({"acquisition_mode":"direct", "api_endpoint":endpoint,
            "store_path":store, "retrieval_policy":"offline_preferred",
            "request_deadline_ms":3000, "max_concurrent_requests":1}),
        artifacts: vec![],
        resources: vec![],
        anchors: vec![seed],
    };
    let start = Instant::now();
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        serde_json::to_string(&request).unwrap()
    )
    .unwrap();
    let mut response = String::new();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    reader.read_line(&mut response).unwrap();
    let elapsed = start.elapsed();
    match serde_json::from_str::<GuidanceResponse>(&response).unwrap() {
        GuidanceResponse::Prepared { diagnostics, .. } => assert_eq!(
            diagnostics.request_count, 1,
            "the current job must wait for its missing artist lookup"
        ),
        other => panic!("expected prepared response, got {other:?}"),
    }
    let score = GuidanceRequest::Score {
        spi_version: SPI_VERSION,
        request_id: "score-fixture".into(),
        context: ScoreContext {
            scope: GuidanceScope::Edge,
            left_anchor_id: Some("seed".into()),
            right_anchor_id: None,
            context_track_ids: vec![],
        },
        candidates: vec![candidate()],
    };
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        serde_json::to_string(&score).unwrap()
    )
    .unwrap();
    response.clear();
    reader.read_line(&mut response).unwrap();
    match serde_json::from_str::<GuidanceResponse>(&response).unwrap() {
        GuidanceResponse::Scores { signals, .. } => assert!(
            !signals.is_empty(),
            "online similarity must influence the current job"
        ),
        other => panic!("expected scores, got {other:?}"),
    }
    drop(child.stdin.take());
    child.wait().unwrap();
    assert!(
        elapsed < Duration::from_millis(1000),
        "the online response took unexpectedly long: {elapsed:?}"
    );
    assert!(
        server.join().unwrap(),
        "background refresh never contacted the server"
    );
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let graph = bliss_guidance_lastfm::store::GraphStore::open(&store).unwrap();
        let id = graph
            .find_artist(&bliss_guidance_lastfm::store::ArtistIdentity {
                name: "Seed Artist".into(),
                mbid: Some("seed-artist".into()),
            })
            .unwrap();
        if id.is_some_and(|id| graph.artist_observation(id).unwrap().is_some()) {
            break;
        }
        assert!(
            Instant::now() < until,
            "background refresh did not persist its result"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn locked_database_does_not_delay_online_guidance_response() {
    use bliss_playlist_guidance_spi::{GuidanceRequest, GuidanceResponse, SPI_VERSION};
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Instant;
    let store = store_path();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let database = store.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request).unwrap();
        let lock = rusqlite::Connection::open(database).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let body = r#"{"similarartists":{"artist":[{"name":"Related Artist","mbid":"related-artist","match":"0.8"}]}}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=3600\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        thread::sleep(Duration::from_millis(2500));
        lock.execute_batch("ROLLBACK").unwrap();
    });
    let mut seed = anchor();
    seed.track.title = None;
    let mut child = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .env(API_KEY_ENVIRONMENT, "test-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let request = GuidanceRequest::Prepare {
        spi_version: SPI_VERSION,
        job_id: "locked-fixture".into(),
        options: serde_json::json!({"acquisition_mode":"direct", "api_endpoint":endpoint,
            "store_path":store, "retrieval_policy":"offline_preferred", "request_deadline_ms":3000, "max_concurrent_requests":1}),
        artifacts: vec![],
        resources: vec![],
        anchors: vec![seed],
    };
    let start = Instant::now();
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        serde_json::to_string(&request).unwrap()
    )
    .unwrap();
    let mut response = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut response)
        .unwrap();
    let elapsed = start.elapsed();
    drop(child.stdin.take());
    child.wait().unwrap();
    server.join().unwrap();
    assert!(matches!(
        serde_json::from_str::<GuidanceResponse>(&response).unwrap(),
        GuidanceResponse::Prepared { .. }
    ));
    assert!(
        elapsed < Duration::from_millis(1500),
        "database persistence delayed the response by {elapsed:?}"
    );
}

#[test]
fn failed_background_persistence_leaves_a_report_for_lyrion() {
    use std::process::{Command, Stdio};
    let directory = store_path().with_extension("fixture-dir");
    std::fs::create_dir(&directory).unwrap();
    let graph = directory.join("graph.sqlite");
    std::fs::create_dir(&graph).unwrap();
    let mut worker = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .arg("persist-guidance-background")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = serde_json::json!({"options": {"acquisition_mode":"direct",
        "store_path":graph, "request_deadline_ms":1000, "max_concurrent_requests":1}, "writes":[]});
    worker
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&payload).unwrap().as_bytes())
        .unwrap();
    assert!(!worker.wait().unwrap().success());
    let entries: Vec<_> = std::fs::read_dir(directory.join("persistence-errors"))
        .unwrap()
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "one durable error report is available to the Lyrion logger"
    );
    let report = std::fs::read(entries[0].as_ref().unwrap().path()).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&report).unwrap();
    assert!(value["error"].as_str().unwrap().contains("Last.fm"));
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
    let timings = &diagnostics.details.as_ref().unwrap()["durations_ms"];
    assert!(
        timings["local_lookup"].is_u64(),
        "local cache lookup timing is returned"
    );
    assert!(
        timings["online_fetch"].is_u64(),
        "online acquisition timing is returned"
    );
    assert!(
        timings["total"].is_u64(),
        "total preparation timing is returned"
    );
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
fn stale_artist_data_remains_usable_when_online_refresh_fails() {
    use bliss_guidance_lastfm::store::{
        ArtistEdgeWrite, ArtistObservationWrite, GraphStore, ObservationStatus,
    };
    let _guard = API_KEY_LOCK.lock().unwrap();
    std::env::set_var(API_KEY_ENVIRONMENT, "test-key");
    let path = store_path();
    let mut graph = GraphStore::open(&path).unwrap();
    let source = graph
        .upsert_artist(Some("seed-artist"), "Seed Artist", 1)
        .unwrap();
    let target = graph
        .upsert_artist(Some("related-artist"), "Related Artist", 1)
        .unwrap();
    graph
        .replace_artist_observation(
            source,
            &ArtistObservationWrite {
                fetched_at: 1,
                validated_at: 1,
                changed_at: 1,
                expires_at: 2,
                etag: None,
                last_modified: None,
                payload_hash: "fixture".into(),
                acquisition_source: "api_key".into(),
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
    drop(graph);
    let mut seed = anchor();
    seed.track.title = None;
    let (prepared, diagnostics) = DirectPrepared::prepare_deferred(
        &options("http://127.0.0.1:9", &path, "offline_preferred"),
        &[seed],
    )
    .unwrap();
    assert!(diagnostics.failure_count > 0);
    let context = ScoreContext {
        scope: GuidanceScope::Edge,
        left_anchor_id: Some("seed".into()),
        right_anchor_id: None,
        context_track_ids: vec![],
    };
    assert_eq!(
        prepared.signals(&context, &[candidate()]).len(),
        1,
        "the existing artist relation survives the failed request"
    );
    std::env::remove_var(API_KEY_ENVIRONMENT);
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
