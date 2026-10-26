use bliss_guidance_lastfm::legacy_cache::{migrate_legacy_cache, MigrationOutcome};
use bliss_guidance_lastfm::store::{ArtistIdentity, GraphStore, ObservationStatus};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_path(label: &str, extension: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bliss-guidance-lastfm-migration-{label}-{}-{nonce}.{extension}",
        std::process::id()
    ))
}

fn copy_fixture(name: &str, target: &PathBuf) {
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name),
        target,
    )
    .unwrap();
}

#[test]
fn imports_artist_entries_and_preserves_track_entries_outside_sqlite() {
    let legacy = temporary_path("valid", "json");
    let database = temporary_path("valid", "sqlite");
    copy_fixture("lastfm-cache-v1.json", &legacy);
    let mut store = GraphStore::open_at(&database, 1_600_000_000).unwrap();

    let outcome = migrate_legacy_cache(&legacy, &mut store, 1_800_000_000).unwrap();
    assert_eq!(
        outcome,
        MigrationOutcome::Imported {
            entries: 3,
            artist_observations: 2,
            preserved_track_entries: 1,
        }
    );
    assert!(legacy.exists());
    assert_eq!(store.migrated_from_json_at().unwrap(), Some(1_800_000_000));

    let source = store
        .find_artist(&ArtistIdentity {
            mbid: None,
            name: "Seed Artist".to_owned(),
        })
        .unwrap()
        .unwrap();
    let positive = store.artist_observation(source).unwrap().unwrap();
    assert_eq!(positive.fetched_at, 1_700_000_000);
    assert_eq!(positive.result_status, ObservationStatus::Positive);
    assert_eq!(positive.edges.len(), 1);

    let empty_source = store
        .find_artist(&ArtistIdentity {
            mbid: None,
            name: "Empty Artist".to_owned(),
        })
        .unwrap()
        .unwrap();
    let empty = store.artist_observation(empty_source).unwrap().unwrap();
    assert_eq!(empty.fetched_at, 1_700_000_100);
    assert_eq!(empty.result_status, ObservationStatus::Empty);
    assert!(empty.edges.is_empty());

    let retained: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&legacy).unwrap()).unwrap();
    let entries = retained["entries"].as_object().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries.contains_key("track:seed artist|seed song"));
    assert!(!store.has_table("tracks").unwrap());

    assert_eq!(
        migrate_legacy_cache(&legacy, &mut store, 1_900_000_000).unwrap(),
        MigrationOutcome::AlreadyMigrated
    );
    assert_eq!(store.migrated_from_json_at().unwrap(), Some(1_800_000_000));

    drop(store);
    let _ = std::fs::remove_file(legacy);
    let _ = std::fs::remove_file(database);
}

#[test]
fn preserves_a_corrupt_cache_for_manual_inspection() {
    let legacy = temporary_path("corrupt", "json");
    let database = temporary_path("corrupt", "sqlite");
    copy_fixture("lastfm-cache-v1-corrupt.json", &legacy);
    let mut store = GraphStore::open(&database).unwrap();

    let outcome = migrate_legacy_cache(&legacy, &mut store, 1_800_000_000).unwrap();
    assert!(matches!(outcome, MigrationOutcome::PreservedInvalid { .. }));
    assert!(legacy.exists());
    assert_eq!(store.migrated_from_json_at().unwrap(), None);

    drop(store);
    let _ = std::fs::remove_file(legacy);
    let _ = std::fs::remove_file(database);
}
