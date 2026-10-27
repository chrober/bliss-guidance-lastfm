use bliss_guidance_lastfm::store::GraphStore;
use rusqlite::Connection;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn library_membership_refresh_tracks_additions_removals_and_distinct_mbids() {
    let dir = std::env::temp_dir().join(format!(
        "lastfm-membership-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let library = dir.join("library.db");
    let graph = dir.join("graph.db");
    let db = Connection::open(&library).unwrap();
    db.execute_batch("CREATE TABLE contributors(id INTEGER PRIMARY KEY,name TEXT,musicbrainz_id TEXT); CREATE TABLE tracks(id INTEGER PRIMARY KEY,remote INTEGER); CREATE TABLE contributor_track(contributor INTEGER,track INTEGER); INSERT INTO tracks VALUES(1,0),(2,1); INSERT INTO contributors VALUES(1,'First','mbid-1'),(2,'Shared','mbid-2'),(3,'Shared','mbid-3'),(4,'Remote',NULL); INSERT INTO contributor_track VALUES(1,1),(2,1),(3,1),(4,2);").unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .arg("refresh-library-membership")
        .arg(&library)
        .arg(&graph)
        .status()
        .unwrap();
    assert!(status.success());
    let store = GraphStore::open(&graph).unwrap();
    assert_eq!(store.statistics().unwrap().library_artists, 3);
    drop(store);
    db.execute_batch("DELETE FROM contributor_track WHERE contributor=1; INSERT INTO contributors VALUES(5,'New Artist',NULL); INSERT INTO contributor_track VALUES(5,1);").unwrap();
    bliss_guidance_lastfm::library_membership::refresh(&library, &graph).unwrap();
    let store = GraphStore::open(&graph).unwrap();
    assert_eq!(store.statistics().unwrap().library_artists, 3);
    let new = store
        .find_artist(&bliss_guidance_lastfm::store::ArtistIdentity {
            mbid: None,
            name: "New Artist".to_owned(),
        })
        .unwrap()
        .unwrap();
    assert!(store.is_library_artist(new).unwrap());
    let old = store
        .find_artist(&bliss_guidance_lastfm::store::ArtistIdentity {
            mbid: Some("mbid-1".into()),
            name: "First".into(),
        })
        .unwrap()
        .unwrap();
    assert!(!store.is_library_artist(old).unwrap());
}

#[test]
fn invalid_library_does_not_replace_the_existing_membership() {
    let dir = std::env::temp_dir().join(format!(
        "lastfm-membership-error-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let graph = dir.join("graph.db");
    let library = dir.join("missing.db");
    let mut store = GraphStore::open(&graph).unwrap();
    let artist = store.upsert_artist(None, "Existing", 1).unwrap();
    store
        .replace_library_membership("old", "old", 1, &[artist])
        .unwrap();
    drop(store);
    assert!(bliss_guidance_lastfm::library_membership::refresh(&library, &graph).is_err());
    let store = GraphStore::open(&graph).unwrap();
    assert!(store.is_library_artist(artist).unwrap());
    assert!(!library.exists());
}

#[test]
fn provider_prepare_refreshes_membership_in_background_without_a_bulk_update() {
    use bliss_guidance_lastfm::store::{ArtistObservationWrite, ObservationStatus};
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!(
        "lastfm-membership-prepare-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let graph = dir.join("graph.db");
    let library = dir.join("library.db");
    let db = Connection::open(&library).unwrap();
    db.execute_batch("CREATE TABLE contributors(id INTEGER PRIMARY KEY,name TEXT,musicbrainz_id TEXT); CREATE TABLE tracks(id INTEGER PRIMARY KEY,remote INTEGER); CREATE TABLE contributor_track(contributor INTEGER,track INTEGER); INSERT INTO tracks VALUES(1,0); INSERT INTO contributors VALUES(1,'Seed Artist','seed-artist'); INSERT INTO contributor_track VALUES(1,1);").unwrap();
    drop(db);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut store = GraphStore::open(&graph).unwrap();
    let id = store
        .upsert_artist(Some("seed-artist"), "Seed Artist", now)
        .unwrap();
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
                payload_hash: "fixture".into(),
                acquisition_source: "api_key".into(),
                requested_limit: 25,
                actual_result_count: 0,
                result_status: ObservationStatus::Empty,
            },
            &[],
        )
        .unwrap();
    drop(store);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .env("BLISS_GUIDANCE_LASTFM_API_KEY", "fixture")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let prepare = serde_json::json!({"type":"prepare", "spi_version":2, "job_id":"test", "options":{
        "acquisition_mode":"direct", "store_path":graph, "library_db_path":library,
        "retrieval_policy":"offline_only", "request_deadline_ms":2000, "max_concurrent_requests":1
    }, "artifacts":[], "resources":[], "anchors":[{"anchor_id":"seed","candidate_id":"seed","artist":"Seed Artist","artist_mbids":["seed-artist"]}]});
    writeln!(child.stdin.take().unwrap(), "{prepare}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"type\":\"prepared\""));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let store = GraphStore::open(&graph).unwrap();
        if store.statistics().unwrap().library_artists == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background refresh did not complete"
        );
        drop(store);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
