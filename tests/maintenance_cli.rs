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
fn status_is_read_only_and_reports_a_missing_or_available_graph() {
    let store = temp_path("status", "sqlite");
    let request_path = request(&store, serde_json::json!({}));
    let output = run("status", &request_path);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "missing");

    bliss_guidance_lastfm::store::GraphStore::open(&store).unwrap();
    let output = run("status", &request_path);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "available");
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(store);
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
fn sync_emits_versioned_progress_and_persists_one_root() {
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
            "artist_result_limit":25
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
    let graph = bliss_guidance_lastfm::store::GraphStore::open(&store).unwrap();
    assert_eq!(graph.statistics().unwrap().artist_observations, 1);
    assert_eq!(graph.statistics().unwrap().artist_edges, 1);
    drop(graph);
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(store);
}
