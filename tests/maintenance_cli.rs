use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_path(label: &str, extension: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "lastfm-maintenance-{label}-{}-{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        extension
    ))
}

fn request(store: &std::path::Path, extra: Value) -> PathBuf {
    let path = temp_path("request", "json");
    let mut value = serde_json::json!({"store_path": store});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().expect("extra request properties").clone());
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    path
}

fn run(command: &str, request: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .args(["graph", command, "--request"])
        .arg(request)
        .env_remove("BLISS_GUIDANCE_LASTFM_API_KEY")
        .output()
        .unwrap()
}

#[test]
fn sync_counts_unique_resolved_library_artists_without_merging_distinct_mbids() {
    use bliss_guidance_lastfm::store::{ArtistObservationWrite, GraphStore, ObservationStatus};
    let store_path = temp_path("unique-roots", "sqlite");
    let roots_path = temp_path("unique-roots", "json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut store = GraphStore::open(&store_path).unwrap();
    for (mbid, name) in [
        (None, "Example Artist"),
        (Some("artist-one"), "Shared Name"),
        (Some("artist-two"), "Shared Name"),
    ] {
        let id = store.upsert_artist(mbid, name, now).unwrap();
        store
            .replace_artist_observation(
                id,
                &ArtistObservationWrite {
                    fetched_at: now,
                    validated_at: now,
                    changed_at: now,
                    expires_at: now + 3600,
                    etag: None,
                    last_modified: None,
                    payload_hash: "fixture".to_owned(),
                    acquisition_source: "api_key".to_owned(),
                    requested_limit: 25,
                    actual_result_count: 0,
                    result_status: ObservationStatus::Empty,
                },
                &[],
            )
            .unwrap();
    }
    drop(store);
    fs::write(&roots_path, serde_json::to_vec(&serde_json::json!({
        "kind":"lastfm-library-artist-roots-v1", "snapshot_id":"unique", "library_identity":"test", "generated_at":now,
        "artists":[
            {"lms_contributor_id":1,"name":"Example Artist","artist_mbids":[]},
            {"lms_contributor_id":2,"name":"EXAMPLE ARTIST","artist_mbids":[]},
            {"lms_contributor_id":3,"name":"Shared Name","artist_mbids":["artist-one"]},
            {"lms_contributor_id":4,"name":"Shared Name","artist_mbids":["artist-two"]}
        ]
    })).unwrap()).unwrap();
    let request_path = request(
        &store_path,
        serde_json::json!({"acquisition_mode":"direct", "roots_path":roots_path}),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .args(["graph", "sync", "--request"])
        .arg(&request_path)
        .env("BLISS_GUIDANCE_LASTFM_API_KEY", "fixture")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let events: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let last = events.last().unwrap();
    assert_eq!(last["total"], 3);
    assert_eq!(last["current"], 3);
    assert_eq!(last["cached"], 3);
    let store = GraphStore::open(&store_path).unwrap();
    let stats = store.statistics().unwrap();
    assert_eq!(stats.library_artists, 3);
    assert_eq!(stats.covered_library_artists, 3);
    drop(store);
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(roots_path);
    let _ = fs::remove_file(store_path);
}

#[test]
fn sync_failure_identifies_artist_and_safe_parse_context() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0_u8; 8192];
        let _ = stream.read(&mut buffer).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .unwrap();
    });
    let store = temp_path("failure-context", "sqlite");
    let roots = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/library-artist-roots-v1.json");
    let request_path = request(
        &store,
        serde_json::json!({
            "acquisition_mode":"direct", "roots_path":roots, "api_endpoint":endpoint
        }),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .args(["graph", "sync", "--request"])
        .arg(&request_path)
        .env("BLISS_GUIDANCE_LASTFM_API_KEY", "secret-test-key")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.contains("artist.getSimilar for Seed Artist (1/1)"),
        "{output}"
    );
    assert!(output.contains("stage=result_shape"), "{output}");
    assert!(!output.contains("secret-test-key"));
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(store);
}

#[test]
fn custom_empty_target_revalidates_a_fresh_cached_observation_conditionally() {
    use bliss_guidance_lastfm::store::{ArtistObservationWrite, GraphStore, ObservationStatus};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    let store_path = temp_path("target-revalidate", "sqlite");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut store = GraphStore::open(&store_path).unwrap();
    let id = store
        .upsert_artist(Some("seed-artist-mbid"), "Seed Artist", now)
        .unwrap();
    store
        .replace_artist_observation(
            id,
            &ArtistObservationWrite {
                fetched_at: now - 10 * 86400,
                validated_at: now - 10 * 86400,
                changed_at: now - 10 * 86400,
                expires_at: now + 30 * 86400,
                etag: Some("\"fixture-etag\"".to_owned()),
                last_modified: None,
                payload_hash: "fixture".to_owned(),
                acquisition_source: "api_key".to_owned(),
                requested_limit: 25,
                actual_result_count: 0,
                result_status: ObservationStatus::Empty,
            },
            &[],
        )
        .unwrap();
    drop(store);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0_u8; 8192];
        let n = stream.read(&mut buffer).unwrap();
        let request = String::from_utf8_lossy(&buffer[..n]).to_ascii_lowercase();
        assert!(
            request.contains("if-none-match: \"fixture-etag\""),
            "{request}"
        );
        stream.write_all(b"HTTP/1.1 304 Not Modified\r\nCache-Control: max-age=3600\r\nConnection: close\r\n\r\n").unwrap();
    });
    let roots = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/library-artist-roots-v1.json");
    let request_path = request(
        &store_path,
        serde_json::json!({"acquisition_mode":"direct", "roots_path":roots, "api_endpoint":endpoint, "empty_artist_refresh_days":7}),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .args(["graph", "sync", "--request"])
        .arg(&request_path)
        .env("BLISS_GUIDANCE_LASTFM_API_KEY", "fixture-key")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let store = GraphStore::open(&store_path).unwrap();
    let observation = store.artist_observation(id).unwrap().unwrap();
    assert!(observation.validated_at >= now);
    assert_eq!(observation.result_status, ObservationStatus::Empty);
    drop(store);
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(store_path);
}

#[test]
fn compact_uses_current_library_snapshot_and_reports_targeted_cleanup() {
    use bliss_guidance_lastfm::store::GraphStore;
    let store_path = temp_path("cleanup", "sqlite");
    let mut store = GraphStore::open(&store_path).unwrap();
    let source = store
        .upsert_artist(Some("seed-artist-mbid"), "BjÃ¶rk", 10)
        .unwrap();
    let orphan = store.upsert_artist(None, "Unused", 10).unwrap();
    store
        .replace_library_membership("old", "old", 10, &[source, orphan])
        .unwrap();
    drop(store);
    let roots_path = temp_path("roots", "json");
    fs::write(&roots_path, serde_json::to_vec(&serde_json::json!({
        "kind":"lastfm-library-artist-roots-v1", "snapshot_id":"fresh", "library_identity":"l", "generated_at":20,
        "artists":[{"lms_contributor_id":1,"name":"Björk","artist_mbids":["seed-artist-mbid"]}]
    })).unwrap()).unwrap();
    let request_path = request(
        &store_path,
        serde_json::json!({"acquisition_mode":"direct", "roots_path":roots_path}),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .args(["graph", "compact", "--request"])
        .arg(&request_path)
        .env("BLISS_GUIDANCE_LASTFM_API_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events.iter().any(|event| event["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())),
        "cleanup must report its progress before finishing"
    );
    let result = events.last().unwrap();
    assert_eq!(result["repaired_names"], 1);
    assert_eq!(result["removed_orphans"], 1);
    let store = GraphStore::open(&store_path).unwrap();
    assert_eq!(
        store.artist(source).unwrap().unwrap().canonical_name,
        "Björk"
    );
    assert!(store.artist(orphan).unwrap().is_none());
    drop(store);
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(roots_path);
    let _ = fs::remove_file(store_path);
}

#[test]
fn local_only_cleanup_removes_unreferenced_same_name_and_trimmed_artists() {
    use bliss_guidance_lastfm::store::{
        ArtistEdgeWrite, ArtistObservationWrite, GraphStore, ObservationStatus,
    };
    let store_path = temp_path("cleanup-local", "sqlite");
    let roots_path = temp_path("cleanup-local-roots", "json");
    let mut store = GraphStore::open(&store_path).unwrap();
    let local = store
        .upsert_artist(Some("local-id"), "Shared Name", 10)
        .unwrap();
    let duplicate = store
        .upsert_artist(Some("external-id"), "Shared Name", 10)
        .unwrap();
    let target = store.upsert_artist(None, "External target", 10).unwrap();
    store
        .replace_library_membership("old", "old", 10, &[local])
        .unwrap();
    store
        .replace_artist_observation(
            local,
            &ArtistObservationWrite {
                fetched_at: 10,
                validated_at: 10,
                changed_at: 10,
                expires_at: 100,
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
    store
        .replace_artist_observation(
            duplicate,
            &ArtistObservationWrite {
                fetched_at: 10,
                validated_at: 10,
                changed_at: 10,
                expires_at: 100,
                etag: None,
                last_modified: None,
                payload_hash: "external".into(),
                acquisition_source: "api_key".into(),
                requested_limit: 25,
                actual_result_count: 0,
                result_status: ObservationStatus::Empty,
            },
            &[],
        )
        .unwrap();
    drop(store);
    fs::write(&roots_path, serde_json::to_vec(&serde_json::json!({
        "kind":"lastfm-library-artist-roots-v1", "snapshot_id":"fresh", "library_identity":"l", "generated_at":20,
        "artists":[{"lms_contributor_id":1,"name":"Shared Name","artist_mbids":["local-id"]}]
    })).unwrap()).unwrap();
    let request_path = request(
        &store_path,
        serde_json::json!({"acquisition_mode":"direct", "roots_path":roots_path, "storage_scope":"local_artists_only"}),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .args(["graph", "compact", "--request"])
        .arg(&request_path)
        .env("BLISS_GUIDANCE_LASTFM_API_KEY", "fixture")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let store = GraphStore::open(&store_path).unwrap();
    assert!(store.artist(local).unwrap().is_some());
    assert!(store.artist(duplicate).unwrap().is_none());
    assert!(store.artist(target).unwrap().is_none());
    assert_eq!(store.statistics().unwrap().external_artists, 0);
}

#[test]
fn cleanup_reports_progress_while_waiting_for_another_database_writer() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let store_path = temp_path("locked-cleanup", "sqlite");
    drop(bliss_guidance_lastfm::store::GraphStore::open(&store_path).unwrap());
    let connection = rusqlite::Connection::open(&store_path).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let request_path = request(
        &store_path,
        serde_json::json!({"acquisition_mode":"direct"}),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .args(["graph", "compact", "--request"])
        .arg(&request_path)
        .env("BLISS_GUIDANCE_LASTFM_API_KEY", "fixture")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let event: Value = serde_json::from_str(&first).unwrap();
    assert!(event["message"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
    assert!(
        child.try_wait().unwrap().is_none(),
        "cleanup waits while another writer holds the database"
    );
    std::thread::sleep(std::time::Duration::from_secs(6));
    connection.execute_batch("ROLLBACK").unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn status_is_read_only_and_reports_a_missing_or_available_graph() {
    let store = temp_path("status", "sqlite");
    let legacy_cache = temp_path("legacy-status", "json");
    fs::write(&legacy_cache, br#"{"schema_version":1,"entries":{"artist:test":{"fetched_at_unix_seconds":1,"value":[{"name":"AB"}]}}}"#).unwrap();
    let request_path = request(
        &store,
        serde_json::json!({"legacy_cache_path":legacy_cache}),
    );
    let output = run("status", &request_path);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "missing");
    assert_eq!(value["database_bytes"], 0);
    assert_eq!(value["retained_lastfm_bytes"], 2);

    bliss_guidance_lastfm::store::GraphStore::open(&store).unwrap();
    let output = run("status", &request_path);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "available");
    assert!(value["database_bytes"].as_u64().unwrap() > 0);
    assert_eq!(value["retained_lastfm_bytes"], 2);
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(store);
    let _ = fs::remove_file(legacy_cache);
}

#[test]
fn unreadable_cache_is_preserved_and_accounting_reports_an_error() {
    let store = temp_path("invalid-status", "sqlite");
    let cache = temp_path("invalid-cache", "json");
    fs::write(&cache, b"invalid cache content").unwrap();
    let request_path = request(&store, serde_json::json!({"legacy_cache_path":cache}));
    let output = run("status", &request_path);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("cannot account unrecognized Last.fm cache"));
    assert_eq!(fs::read(&cache).unwrap(), b"invalid cache content");
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(&cache);
    let _ = fs::remove_file(cache.with_extension("lock"));
}

#[test]
fn graph_status_prunes_expired_track_cache_before_reporting_retained_payload() {
    use bliss_guidance_lastfm::lastfm_api::SimilarTrack;
    let store = temp_path("status-track-data", "sqlite");
    let cache = temp_path("status-track-cache", "json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let track = SimilarTrack {
        artist: "Artist".into(),
        title: "Title".into(),
        artist_mbid: None,
        recording_mbid: None,
        score: Some(0.5),
    };
    fs::write(
        &cache,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version":1, "entries":{
                "track:expired":{"fetched_at_unix_seconds":now-90000,"value":[track]},
                "track:current":{"fetched_at_unix_seconds":now,"value":[track]}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let request_path = request(&store, serde_json::json!({"legacy_cache_path":cache}));
    let output = run("status", &request_path);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["retained_lastfm_bytes"],
        (track.artist.len() + track.title.len() + "0.5".len()) as u64
    );
    let persisted: Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    assert!(persisted["entries"].get("track:expired").is_none());
    assert!(persisted["entries"].get("track:current").is_some());
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(&cache);
    let _ = fs::remove_file(cache.with_extension("lock"));
}

#[test]
fn mutating_commands_require_api_key_mode_and_environment_except_confirmed_delete() {
    let store = temp_path("guard", "sqlite");
    bliss_guidance_lastfm::store::GraphStore::open(&store).unwrap();
    let roots = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/library-artist-roots-v1.json");
    let request_path = request(
        &store,
        serde_json::json!({"acquisition_mode":"direct","roots_path":roots}),
    );
    for command in ["sync", "compact"] {
        let output = run(command, &request_path);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("API key"));
    }

    let delete_request = request(&store, serde_json::json!({"confirmed":true}));
    let output = run("delete", &delete_request);
    assert!(output.status.success());
    assert!(!store.exists());
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(delete_request);
}

#[test]
fn sync_emits_versioned_progress_and_strictly_respects_the_budget() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0_u8; 8192];
        let size = stream.read(&mut buffer).unwrap();
        assert!(String::from_utf8_lossy(&buffer[..size]).contains("artist.getSimilar"));
        let body = r#"{"similarartists":{"artist":[{"name":"Target Artist","mbid":"target-mbid","match":"0.9"}]}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let store = temp_path("sync", "sqlite");
    let roots = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/library-artist-roots-v1.json");
    let request_path = request(
        &store,
        serde_json::json!({
            "acquisition_mode":"direct",
            "roots_path":roots,
            "api_endpoint":endpoint,
            "artist_result_limit":25,
            "budget_bytes":1
        }),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .args(["graph", "sync", "--request"])
        .arg(&request_path)
        .env("BLISS_GUIDANCE_LASTFM_API_KEY", "test-key")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let events = String::from_utf8(output.stdout).unwrap();
    assert!(events.contains("lastfm_graph_progress_v1"));
    assert!(events.contains("\"finished\":true"));
    assert!(events.contains("\"budget_exhausted\":true"));
    let first_event: Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
    assert_eq!(first_event["current"], 0);
    assert_eq!(first_event["total"], 0);
    assert!(first_event["message"]
        .as_str()
        .unwrap()
        .starts_with("Preparing library artists"));
    let parsed_events: Vec<Value> = events
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(parsed_events.iter().any(|event| event["total"] == 1));
    let fetching_event = parsed_events
        .iter()
        .find(|event| {
            event["message"]
                .as_str()
                .is_some_and(|message| message.contains("Fetching similar artists for Seed Artist"))
        })
        .expect("sync progress names the artist currently being fetched in its status message");
    assert_eq!(fetching_event["current"], 0);
    assert_eq!(fetching_event["retained_lastfm_bytes"], 0);
    let last_event: Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
    assert_eq!(last_event["current"], 0);
    assert_eq!(last_event["retained_lastfm_bytes"], 0);
    let graph = bliss_guidance_lastfm::store::GraphStore::open(&store).unwrap();
    assert_eq!(graph.statistics().unwrap().artist_observations, 0);
    assert_eq!(graph.statistics().unwrap().artist_edges, 0);
    assert_eq!(graph.retained_lastfm_bytes().unwrap(), 0);
    drop(graph);
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(store);
}
