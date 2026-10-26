use bliss_guidance_lastfm::store::{
    ArtistEdgeWrite, ArtistObservationWrite, GraphStore, ObservationStatus,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn database_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bliss-guidance-lastfm-{label}-{}-{nonce}.sqlite",
        std::process::id()
    ))
}

fn observation(payload_hash: &str, status: ObservationStatus) -> ArtistObservationWrite {
    ArtistObservationWrite {
        fetched_at: 1_700_000_000,
        validated_at: 1_700_000_001,
        changed_at: 1_700_000_002,
        expires_at: 1_700_003_600,
        etag: Some("etag-one".to_owned()),
        last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
        payload_hash: payload_hash.to_owned(),
        acquisition_source: "api_key".to_owned(),
        requested_limit: 25,
        actual_result_count: 1,
        result_status: status,
    }
}

#[test]
fn creates_versioned_schema_with_wal_and_busy_timeout() {
    let path = database_path("schema");
    let store = GraphStore::open(&path).unwrap();

    assert_eq!(store.schema_version().unwrap(), 2);
    assert_eq!(store.journal_mode().unwrap(), "wal");
    assert!(store.busy_timeout_millis().unwrap() >= 5_000);
    for table in [
        "metadata",
        "artists",
        "artist_aliases",
        "artist_queries",
        "artist_similarity_edges",
        "library_snapshots",
        "library_artist_membership",
    ] {
        assert!(store.has_table(table).unwrap(), "missing table {table}");
    }
    for table in ["tracks", "track_queries", "track_similarity_edges"] {
        assert!(!store.has_table(table).unwrap(), "unexpected table {table}");
    }

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn accounts_only_the_canonical_lastfm_payload_retained_by_an_observation() {
    let path = database_path("retained-payload");
    let mut store = GraphStore::open(&path).unwrap();
    let source = store
        .upsert_artist(None, "Local Source", 1_700_000_000)
        .unwrap();
    let target = store
        .upsert_artist(Some("target-mbid"), "Target Artist", 1_700_000_000)
        .unwrap();

    store
        .replace_artist_observation(
            source,
            &observation("payload-one", ObservationStatus::Positive),
            &[ArtistEdgeWrite {
                target_artist_id: target,
                rank: 1,
                match_score: Some(0.91),
            }],
        )
        .unwrap();

    let expected = serde_json::to_vec(&serde_json::json!({
        "etag": "etag-one",
        "last_modified": "Wed, 21 Oct 2015 07:28:00 GMT",
        "results": [{
            "mbid": "target-mbid",
            "name": "Target Artist",
            "rank": 1,
            "score": 0.91
        }]
    }))
    .unwrap()
    .len() as u64;
    assert_eq!(store.retained_lastfm_bytes().unwrap(), expected);

    let mut empty = observation("payload-empty", ObservationStatus::Empty);
    empty.actual_result_count = 0;
    store
        .replace_artist_observation(source, &empty, &[])
        .unwrap();
    let expected_empty = serde_json::to_vec(&serde_json::json!({
        "etag": "etag-one",
        "last_modified": "Wed, 21 Oct 2015 07:28:00 GMT",
        "results": []
    }))
    .unwrap()
    .len() as u64;
    assert_eq!(store.retained_lastfm_bytes().unwrap(), expected_empty);

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn migrates_v1_observations_and_backfills_retained_lastfm_bytes() {
    let path = database_path("v1-retained-migration");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            r#"
            CREATE TABLE metadata (
                metadata_id INTEGER PRIMARY KEY CHECK (metadata_id = 1),
                schema_version INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                migrated_from_json_at INTEGER
            );
            INSERT INTO metadata VALUES (1, 1, 1700000000, NULL);
            CREATE TABLE artists (
                artist_id INTEGER PRIMARY KEY,
                artist_mbid TEXT,
                canonical_name TEXT NOT NULL,
                normalized_name TEXT NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL
            );
            INSERT INTO artists VALUES (1, NULL, 'Local Source', 'local source', 1700000000, 1700000000);
            INSERT INTO artists VALUES (2, 'target-mbid', 'Target Artist', 'target artist', 1700000000, 1700000000);
            CREATE TABLE artist_queries (
                query_id INTEGER PRIMARY KEY,
                source_artist_id INTEGER NOT NULL UNIQUE REFERENCES artists(artist_id) ON DELETE CASCADE,
                fetched_at INTEGER NOT NULL,
                validated_at INTEGER NOT NULL,
                changed_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                etag TEXT,
                last_modified TEXT,
                payload_hash TEXT NOT NULL,
                acquisition_source TEXT NOT NULL,
                requested_limit INTEGER NOT NULL,
                actual_result_count INTEGER NOT NULL,
                result_status TEXT NOT NULL CHECK (result_status IN ('positive', 'empty'))
            );
            INSERT INTO artist_queries VALUES (
                1, 1, 1700000000, 1700000001, 1700000002, 1700003600,
                'etag-one', 'Wed, 21 Oct 2015 07:28:00 GMT', 'legacy-hash',
                'api_key', 25, 1, 'positive'
            );
            CREATE TABLE artist_similarity_edges (
                query_id INTEGER NOT NULL REFERENCES artist_queries(query_id) ON DELETE CASCADE,
                target_artist_id INTEGER NOT NULL REFERENCES artists(artist_id) ON DELETE RESTRICT,
                rank INTEGER NOT NULL CHECK (rank > 0),
                match_score REAL,
                PRIMARY KEY (query_id, target_artist_id)
            );
            INSERT INTO artist_similarity_edges VALUES (1, 2, 1, 0.91);
            CREATE TABLE tracks (
                track_id INTEGER PRIMARY KEY,
                recording_mbid TEXT,
                artist_id INTEGER REFERENCES artists(artist_id),
                canonical_title TEXT NOT NULL,
                normalized_title TEXT NOT NULL
            );
            CREATE TABLE track_queries (
                query_id INTEGER PRIMARY KEY,
                source_track_id INTEGER NOT NULL UNIQUE REFERENCES tracks(track_id),
                fetched_at INTEGER NOT NULL,
                validated_at INTEGER NOT NULL,
                changed_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                etag TEXT,
                last_modified TEXT,
                payload_hash TEXT NOT NULL,
                acquisition_source TEXT NOT NULL,
                requested_limit INTEGER NOT NULL,
                actual_result_count INTEGER NOT NULL,
                result_status TEXT NOT NULL
            );
            CREATE TABLE track_similarity_edges (
                query_id INTEGER NOT NULL REFERENCES track_queries(query_id),
                target_track_id INTEGER NOT NULL REFERENCES tracks(track_id),
                rank INTEGER NOT NULL,
                match_score REAL,
                PRIMARY KEY (query_id, target_track_id)
            );
            "#,
        )
        .unwrap();
    drop(connection);

    let store = GraphStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 2);
    assert!(!store.has_table("tracks").unwrap());
    assert!(!store.has_table("track_queries").unwrap());
    assert!(!store.has_table("track_similarity_edges").unwrap());
    assert!(store.retained_lastfm_bytes().unwrap() > 0);
    let stored = store.artist_observation(1).unwrap().unwrap();
    assert_eq!(stored.payload_hash, "legacy-hash");
    assert_eq!(stored.edges.len(), 1);

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn rejects_a_database_with_a_newer_schema_version() {
    let path = database_path("future-schema");
    {
        let _store = GraphStore::open(&path).unwrap();
    }
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE metadata SET schema_version = 99", [])
        .unwrap();

    let error = GraphStore::open(&path).unwrap_err();
    assert!(error.contains("unsupported Last.fm graph schema version 99"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn atomically_replaces_positive_and_empty_artist_observations() {
    let path = database_path("replace");
    let mut store = GraphStore::open(&path).unwrap();
    let source = store
        .upsert_artist(Some("source-mbid"), "Source Artist", 1_700_000_000)
        .unwrap();
    let target = store
        .upsert_artist(Some("target-mbid"), "Target Artist", 1_700_000_000)
        .unwrap();

    store
        .replace_artist_observation(
            source,
            &observation("hash-one", ObservationStatus::Positive),
            &[ArtistEdgeWrite {
                target_artist_id: target,
                rank: 1,
                match_score: Some(0.91),
            }],
        )
        .unwrap();
    let stored = store.artist_observation(source).unwrap().unwrap();
    assert_eq!(stored.payload_hash, "hash-one");
    assert_eq!(stored.result_status, ObservationStatus::Positive);
    assert_eq!(stored.edges.len(), 1);
    assert_eq!(stored.edges[0].target_artist_id, target);

    let mut empty = observation("hash-empty", ObservationStatus::Empty);
    empty.actual_result_count = 0;
    store
        .replace_artist_observation(source, &empty, &[])
        .unwrap();
    let stored = store.artist_observation(source).unwrap().unwrap();
    assert_eq!(stored.payload_hash, "hash-empty");
    assert_eq!(stored.result_status, ObservationStatus::Empty);
    assert!(stored.edges.is_empty());

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn rolls_back_observation_and_edges_when_replacement_fails() {
    let path = database_path("rollback");
    let mut store = GraphStore::open(&path).unwrap();
    let source = store
        .upsert_artist(None, "Source Artist", 1_700_000_000)
        .unwrap();
    let target = store
        .upsert_artist(None, "Target Artist", 1_700_000_000)
        .unwrap();
    store
        .replace_artist_observation(
            source,
            &observation("stable-hash", ObservationStatus::Positive),
            &[ArtistEdgeWrite {
                target_artist_id: target,
                rank: 1,
                match_score: Some(0.8),
            }],
        )
        .unwrap();

    let error = store
        .replace_artist_observation(
            source,
            &observation("must-rollback", ObservationStatus::Positive),
            &[ArtistEdgeWrite {
                target_artist_id: i64::MAX,
                rank: 1,
                match_score: Some(0.5),
            }],
        )
        .unwrap_err();
    assert!(error.contains("artist similarity edge"));

    let stored = store.artist_observation(source).unwrap().unwrap();
    assert_eq!(stored.payload_hash, "stable-hash");
    assert_eq!(stored.edges.len(), 1);
    assert_eq!(stored.edges[0].target_artist_id, target);

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn reports_whether_an_artist_belongs_to_the_latest_library_snapshot() {
    let path = database_path("membership");
    let mut store = GraphStore::open(&path).unwrap();
    let member = store.upsert_artist(None, "Member", 1_700_000_000).unwrap();
    let outsider = store
        .upsert_artist(None, "Outsider", 1_700_000_000)
        .unwrap();
    store
        .replace_library_membership("snapshot-1", "library-1", 1_700_000_000, &[member])
        .unwrap();
    store
        .replace_library_membership("snapshot-2", "library-2", 1_700_000_001, &[outsider])
        .unwrap();

    assert!(!store.is_library_artist(member).unwrap());
    assert!(store.is_library_artist(outsider).unwrap());

    drop(store);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let snapshots: i64 = connection
        .query_row("SELECT COUNT(*) FROM library_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    let memberships: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM library_artist_membership",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(snapshots, 1);
    assert_eq!(memberships, 1);
    drop(connection);
    let _ = std::fs::remove_file(path);
}

#[test]
fn library_membership_tolerates_duplicate_canonical_artist_ids() {
    let path = database_path("duplicate-membership");
    let mut store = GraphStore::open(&path).unwrap();
    let member = store
        .upsert_artist(
            Some("11111111-1111-1111-1111-111111111111"),
            "Canonical Artist",
            1_700_000_000,
        )
        .unwrap();

    store
        .replace_library_membership("snapshot-1", "library-1", 1_700_000_000, &[member, member])
        .unwrap();

    drop(store);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let memberships: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM library_artist_membership",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(memberships, 1);
    drop(connection);
    let _ = std::fs::remove_file(path);
}

#[test]
fn compaction_applies_the_current_result_limit_and_local_artist_scope() {
    let path = database_path("compact-scope");
    let mut store = GraphStore::open(&path).unwrap();
    let source = store.upsert_artist(None, "Source", 1_700_000_000).unwrap();
    let member = store.upsert_artist(None, "Member", 1_700_000_000).unwrap();
    let outsider = store
        .upsert_artist(None, "Outsider", 1_700_000_000)
        .unwrap();
    store
        .replace_library_membership("snapshot-1", "library-1", 1_700_000_000, &[source, member])
        .unwrap();
    store
        .replace_artist_observation(
            source,
            &observation("compact-hash", ObservationStatus::Positive),
            &[
                ArtistEdgeWrite {
                    target_artist_id: member,
                    rank: 1,
                    match_score: Some(0.9),
                },
                ArtistEdgeWrite {
                    target_artist_id: outsider,
                    rank: 2,
                    match_score: Some(0.8),
                },
            ],
        )
        .unwrap();
    let retained_before_compaction = store.retained_lastfm_bytes().unwrap();

    store.checkpoint_and_compact(25, true).unwrap();
    let stored = store.artist_observation(source).unwrap().unwrap();
    assert_eq!(stored.edges.len(), 1);
    assert_eq!(stored.edges[0].target_artist_id, member);
    assert!(store.retained_lastfm_bytes().unwrap() < retained_before_compaction);

    drop(store);
    let _ = std::fs::remove_file(path);
}
