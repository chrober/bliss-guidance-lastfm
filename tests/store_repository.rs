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

    assert_eq!(store.schema_version().unwrap(), 1);
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
        "tracks",
        "track_queries",
        "track_similarity_edges",
    ] {
        assert!(store.has_table(table).unwrap(), "missing table {table}");
    }

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
