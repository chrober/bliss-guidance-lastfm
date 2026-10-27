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
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
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
fn status_is_read_only_and_reports_a_missing_or_available_graph() {
    let store = temp_path("status", "sqlite");
    let legacy_cache = temp_path("legacy-status", "json");
    fs::write(&legacy_cache, b"legacy-lastfm-data").unwrap();
    let request_path = request(
        &store,
        serde_json::json!({"legacy_cache_path":legacy_cache}),
    );
    let output = run("status", &request_path);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "missing");
    assert_eq!(value["database_bytes"], 0);
    assert_eq!(value["retained_lastfm_bytes"], 18);

    bliss_guidance_lastfm::store::GraphStore::open(&store).unwrap();
    let output = run("status", &request_path);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "available");
    assert!(value["database_bytes"].as_u64().unwrap() > 0);
    assert_eq!(value["retained_lastfm_bytes"], 18);
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(store);
    let _ = fs::remove_file(legacy_cache);
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
    assert_eq!(first_event["total"], 1);
    assert!(first_event["message"]
        .as_str()
        .unwrap()
        .starts_with("Preparing 1 library artists"));
    let parsed_events: Vec<Value> = events
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
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
