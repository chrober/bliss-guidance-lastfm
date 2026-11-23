use bliss_guidance_lastfm::storage_budget::BudgetLock;
use fs2::FileExt;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn background_scan_write_survives_process_restart_and_drains_afterward() {
    use bliss_guidance_lastfm::deferred_queue::queued_bytes;
    use std::io::Write;
    use std::process::{Command, Stdio};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-restart-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let library = dir.join("library.sqlite");
    let db = rusqlite::Connection::open(&library).unwrap();
    db.execute_batch("CREATE TABLE contributors(id INTEGER PRIMARY KEY,name TEXT,musicbrainz_id TEXT); CREATE TABLE tracks(id INTEGER PRIMARY KEY,remote INTEGER); CREATE TABLE contributor_track(contributor INTEGER,track INTEGER); INSERT INTO tracks VALUES(1,0); INSERT INTO contributors VALUES(1,'Local','local-id'); INSERT INTO contributor_track VALUES(1,1);").unwrap();
    let artist = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Local","mbid":"local-id","score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    let options = serde_json::json!({"acquisition_mode":"direct", "store_path":graph, "storage_scope":"local_artists_only", "budget_bytes":10000, "request_deadline_ms":1000, "max_concurrent_requests":1});
    let mut worker = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .arg("persist-guidance-background")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    worker
        .stdin
        .take()
        .unwrap()
        .write_all(
            &serde_json::to_vec(&serde_json::json!({"options":options,"writes":[artist]})).unwrap(),
        )
        .unwrap();
    assert!(worker.wait().unwrap().success());
    assert!(queued_bytes(&graph).unwrap() > 0);
    let request = dir.join("drain.json");
    let mut drain_options = options.clone();
    drain_options["library_db_path"] = serde_json::json!(library);
    std::fs::write(&request, serde_json::to_vec(&drain_options).unwrap()).unwrap();
    assert!(Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .arg("drain-guidance-background")
        .arg(request)
        .status()
        .unwrap()
        .success());
    assert_eq!(queued_bytes(&graph).unwrap(), 0);
    assert_eq!(
        bliss_guidance_lastfm::store::GraphStore::open(&graph)
            .unwrap()
            .statistics()
            .unwrap()
            .artist_edges,
        1
    );
}

#[test]
fn native_drain_command_consumes_a_trusted_request_without_credentials() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-drain-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let artist = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Other","mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&graph, None, &artist, 10000, 100).unwrap());
    let request = dir.join("drain-request.json");
    std::fs::write(&request, serde_json::to_vec(&serde_json::json!({"acquisition_mode":"direct", "store_path":graph, "storage_scope":"all_returned_artists", "budget_bytes":10000, "request_deadline_ms":1000, "max_concurrent_requests":1})).unwrap()).unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_bliss-guidance-lastfm"))
        .arg("drain-guidance-background")
        .arg(&request)
        .env_remove("BLISS_GUIDANCE_LASTFM_API_KEY")
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(queued_bytes(&graph).unwrap(), 0);
}

#[test]
fn deferred_artist_is_drained_with_current_library_membership() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    use bliss_guidance_lastfm::direct::{drain_queued_artists, DirectOptions};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-drain-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let library = dir.join("library.sqlite");
    let db = rusqlite::Connection::open(&library).unwrap();
    db.execute_batch("CREATE TABLE contributors(id INTEGER PRIMARY KEY,name TEXT,musicbrainz_id TEXT); CREATE TABLE tracks(id INTEGER PRIMARY KEY,remote INTEGER); CREATE TABLE contributor_track(contributor INTEGER,track INTEGER); INSERT INTO tracks VALUES(1,0); INSERT INTO contributors VALUES(1,'Local','local-id'); INSERT INTO contributor_track VALUES(1,1);").unwrap();
    let artist = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Local","mbid":"local-id","score":0.9},{"name":"Outside","mbid":"outside-id","score":0.8}],"requested_limit":25,"actual_result_count":2,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&graph, None, &artist, 10000, 100).unwrap());
    let opts = DirectOptions::from_prepare_options(&serde_json::json!({"acquisition_mode":"direct", "store_path":graph, "storage_scope":"local_artists_only", "library_db_path":library, "budget_bytes":10000, "request_deadline_ms":1000, "max_concurrent_requests":1})).unwrap();
    let reports = drain_queued_artists(&opts).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["stored"], 1);
    assert_eq!(reports[0]["excluded_non_library"], 1);
    assert_eq!(queued_bytes(&graph).unwrap(), 0);
}

#[test]
fn drain_keeps_pending_data_when_current_budget_is_too_small() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    use bliss_guidance_lastfm::direct::{drain_queued_artists, DirectOptions};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-drain-budget-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let artist = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Other","mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&graph, None, &artist, 10000, 100).unwrap());
    let before = queued_bytes(&graph).unwrap();
    let opts = DirectOptions::from_prepare_options(&serde_json::json!({"acquisition_mode":"direct", "store_path":graph, "storage_scope":"all_returned_artists", "budget_bytes":1, "request_deadline_ms":1000, "max_concurrent_requests":1})).unwrap();
    assert!(drain_queued_artists(&opts).is_err());
    assert_eq!(
        queued_bytes(&graph).unwrap(),
        before,
        "budget rejection must not lose pending data"
    );
}

#[test]
fn drain_respects_the_current_artist_result_limit() {
    use bliss_guidance_lastfm::deferred_queue::enqueue_artist;
    use bliss_guidance_lastfm::direct::{drain_queued_artists, DirectOptions};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-limit-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let artist = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"One","mbid":null,"score":0.8},{"name":"Two","mbid":null,"score":0.7}],"requested_limit":25,"actual_result_count":2,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&graph, None, &artist, 10000, 100).unwrap());
    let opts = DirectOptions::from_prepare_options(&serde_json::json!({"acquisition_mode":"direct", "store_path":graph, "artist_result_limit":1, "budget_bytes":10000, "request_deadline_ms":1000, "max_concurrent_requests":1})).unwrap();
    drain_queued_artists(&opts).unwrap();
    let store = bliss_guidance_lastfm::store::GraphStore::open(&graph).unwrap();
    assert_eq!(store.statistics().unwrap().artist_edges, 1);
}

#[test]
fn queued_artist_data_reduces_the_budget_available_to_track_cache() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    use bliss_guidance_lastfm::direct::{persist_pending_writes, DirectOptions};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-accounting-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let cache = dir.join("cache.json");
    let artist = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Other","mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&graph, Some(cache.to_str().unwrap()), &artist, 10000, 100).unwrap());
    let budget = queued_bytes(&graph).unwrap();
    let opts = DirectOptions::from_prepare_options(&serde_json::json!({"acquisition_mode":"direct", "store_path":graph, "legacy_cache_path":cache, "budget_bytes":budget, "request_deadline_ms":1000, "max_concurrent_requests":1})).unwrap();
    let track = serde_json::json!([[{"Track":{"key":"track:fixture","artist":"Seed","title":"Song","recording_mbid":null}}, {"Track":{"status":"Modified","results":[{"artist":"Related","title":"Track","artist_mbid":null,"recording_mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]]);
    assert!(
        persist_pending_writes(&opts, track).is_err(),
        "queued Last.fm data must consume the configured budget"
    );
    assert!(!cache.exists());
}

#[test]
fn queued_artist_data_reduces_the_budget_available_to_graph_writes() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    use bliss_guidance_lastfm::direct::{persist_pending_writes, DirectOptions};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-graph-accounting-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let artist = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Other","mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&graph, None, &artist, 10000, 100).unwrap());
    let budget = queued_bytes(&graph).unwrap();
    let opts = DirectOptions::from_prepare_options(&serde_json::json!({"acquisition_mode":"direct", "store_path":graph, "budget_bytes":budget, "request_deadline_ms":1000, "max_concurrent_requests":1})).unwrap();
    assert!(persist_pending_writes(&opts, serde_json::json!([artist])).is_err());
}

#[test]
fn pending_artist_queue_deduplicates_and_respects_budget() {
    use bliss_guidance_lastfm::deferred_queue::enqueue_artist;
    let dir = std::env::temp_dir().join(format!(
        "lastfm-pending-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let item = serde_json::json!([{"Artist":{"key":"artist:seed","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Other","mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(!enqueue_artist(&graph, None, &item, 1, 100).unwrap());
    assert!(enqueue_artist(&graph, None, &item, 10000, 100).unwrap());
    assert!(enqueue_artist(&graph, None, &item, 10000, 100).unwrap());
    assert_eq!(
        std::fs::read_dir(bliss_guidance_lastfm::deferred_queue::queue_directory(&graph).unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn queues_are_isolated_between_graphs_in_the_same_directory() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-isolated-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let first = dir.join("first.sqlite");
    let second = dir.join("second.sqlite");
    let item = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Other","mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&first, None, &item, 10000, 100).unwrap());
    assert!(queued_bytes(&first).unwrap() > 0);
    assert_eq!(queued_bytes(&second).unwrap(), 0);
}

#[test]
fn queues_are_isolated_for_different_extensions_of_the_same_stem() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-stem-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let first = dir.join("graph.sqlite");
    let second = dir.join("graph.db");
    let item = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[],"requested_limit":25,"actual_result_count":0,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&first, None, &item, 10000, 100).unwrap());
    assert_eq!(queued_bytes(&second).unwrap(), 0);
}

#[test]
fn duplicate_queue_entry_must_match_original_content() {
    use bliss_guidance_lastfm::deferred_queue::enqueue_artist;
    let dir = std::env::temp_dir().join(format!(
        "lastfm-corrupt-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let item = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[],"requested_limit":25,"actual_result_count":0,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&graph, None, &item, 10000, 100).unwrap());
    let path =
        std::fs::read_dir(bliss_guidance_lastfm::deferred_queue::queue_directory(&graph).unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
    std::fs::write(path, b"corrupt").unwrap();
    assert!(
        enqueue_artist(&graph, None, &item, 10000, 100).is_err(),
        "a corrupt duplicate must not be accepted as durable"
    );
}

#[test]
fn complete_interrupted_queue_file_is_counted_and_recovered() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, entries_locked, queued_bytes};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-recover-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let item = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Other","mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    assert!(enqueue_artist(&graph, None, &item, 10000, 100).unwrap());
    let before = queued_bytes(&graph).unwrap();
    let entry =
        std::fs::read_dir(bliss_guidance_lastfm::deferred_queue::queue_directory(&graph).unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
    let tmp = entry.with_extension("tmp-123");
    std::fs::rename(&entry, &tmp).unwrap();
    assert_eq!(
        queued_bytes(&graph).unwrap(),
        before,
        "interrupted complete data still counts against budget"
    );
    let _guard = BudgetLock::acquire(&graph).unwrap();
    let recovered = entries_locked(&graph).unwrap();
    assert_eq!(recovered, vec![entry]);
}

#[test]
fn pending_queue_persists_only_allowlisted_artist_data() {
    use bliss_guidance_lastfm::deferred_queue::enqueue_artist;
    let dir = std::env::temp_dir().join(format!(
        "lastfm-safe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let item = serde_json::json!([{"Artist":{"key":"artist:seed","artist":"Seed","artist_mbid":null,"source_id":123,"existing":{"private":"old-observation"},"api_key":"secret-key"}}, {"Artist":{"status":"Modified","results":[],"requested_limit":25,"actual_result_count":0,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null},"api_key":"secret-key"}}]);
    assert!(enqueue_artist(&graph, None, &item, 10000, 100).unwrap());
    let path =
        std::fs::read_dir(bliss_guidance_lastfm::deferred_queue::queue_directory(&graph).unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
    let contents = std::fs::read_to_string(path).unwrap();
    assert!(!contents.contains("secret-key"));
    assert!(!contents.contains("old-observation"));
}

#[test]
fn budget_lock_serializes_writers_for_the_same_graph() {
    let dir = std::env::temp_dir().join(format!(
        "lastfm-budget-lock-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let guard = BudgetLock::acquire(&graph).unwrap();
    let contender = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(BudgetLock::path_for(&graph))
        .unwrap();
    assert!(
        contender.try_lock_exclusive().is_err(),
        "concurrent budget writer must wait"
    );
    drop(guard);
    contender
        .try_lock_exclusive()
        .expect("dropping the guard releases the lock");
}

#[test]
fn concurrent_queue_writers_cannot_exceed_the_retained_data_limit() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    let dir = std::env::temp_dir().join(format!(
        "lastfm-concurrent-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let mut threads = Vec::new();
    for n in 0..8 {
        let path = graph.clone();
        threads.push(std::thread::spawn(move || {
            let item = serde_json::json!([{"Artist":{"key":format!("a{n}"),"artist":format!("Seed {n}"),"artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":format!("Other {n}"),"mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
            enqueue_artist(&path, None, &item, 300, 100).unwrap()
        }));
    }
    let accepted = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|accepted| *accepted)
        .count();
    assert!(accepted > 0 && accepted < 8);
    assert!(queued_bytes(&graph).unwrap() <= 300);
}

#[test]
fn concurrent_queue_and_track_writers_share_one_budget() {
    use bliss_guidance_lastfm::deferred_queue::{enqueue_artist, queued_bytes};
    use bliss_guidance_lastfm::direct::{persist_pending_writes, DirectOptions};
    use bliss_guidance_lastfm::legacy_cache::retained_cache_bytes;
    let dir = std::env::temp_dir().join(format!(
        "lastfm-mixed-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let graph = dir.join("graph.sqlite");
    let cache = dir.join("cache.json");
    bliss_guidance_lastfm::store::GraphStore::open(&graph).unwrap();
    let artist = serde_json::json!([{"Artist":{"key":"a","artist":"Seed","artist_mbid":null,"source_id":0,"existing":null}}, {"Artist":{"status":"Modified","results":[{"name":"Other","mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]);
    let track = serde_json::json!([[{"Track":{"key":"track:fixture","artist":"Seed","title":"Song","recording_mbid":null}}, {"Track":{"status":"Modified","results":[{"artist":"Related","title":"Track","artist_mbid":null,"recording_mbid":null,"score":0.8}],"requested_limit":25,"actual_result_count":1,"headers":{"cache_control":null,"expires":null,"etag":null,"last_modified":null}}}]]);
    let opts = DirectOptions::from_prepare_options(&serde_json::json!({"acquisition_mode":"direct", "store_path":graph, "legacy_cache_path":cache, "budget_bytes":110, "request_deadline_ms":1000, "max_concurrent_requests":1})).unwrap();
    let graph_for_thread = graph.clone();
    let cache_for_thread = cache.clone();
    let writer = std::thread::spawn(move || {
        enqueue_artist(
            &graph_for_thread,
            cache_for_thread.to_str(),
            &artist,
            110,
            100,
        )
        .unwrap()
    });
    let track_written = persist_pending_writes(&opts, track).is_ok();
    let artist_queued = writer.join().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let used = queued_bytes(&graph).unwrap() + retained_cache_bytes(&cache, now).unwrap();
    assert!(
        track_written || artist_queued,
        "at least one writer must make progress"
    );
    assert!(used > 0);
    assert!(used <= 110, "combined queue and cache used {used} bytes");
}
