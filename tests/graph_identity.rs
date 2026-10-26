use bliss_guidance_lastfm::store::{
    ArtistEdgeWrite, ArtistIdentity, ArtistObservationWrite, GraphStore, ObservationStatus,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn database_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bliss-guidance-lastfm-identity-{label}-{}-{nonce}.sqlite",
        std::process::id()
    ))
}

fn artist(name: &str, mbid: Option<&str>) -> ArtistIdentity {
    ArtistIdentity {
        mbid: mbid.map(str::to_owned),
        name: name.to_owned(),
    }
}

fn observation(hash: &str) -> ArtistObservationWrite {
    ArtistObservationWrite {
        fetched_at: 100,
        validated_at: 101,
        changed_at: 102,
        expires_at: 200,
        etag: None,
        last_modified: None,
        payload_hash: hash.to_owned(),
        acquisition_source: "api_key".to_owned(),
        requested_limit: 25,
        actual_result_count: 1,
        result_status: ObservationStatus::Positive,
    }
}

#[test]
fn mbid_identity_wins_over_names_and_distinct_mbids_never_merge() {
    let path = database_path("mbid");
    let store = GraphStore::open(&path).unwrap();
    let first = store
        .resolve_or_create_artist(&artist("Original Name", Some("MBID-A")), 10)
        .unwrap();
    let renamed = store
        .resolve_or_create_artist(&artist("Renamed Artist", Some("mbid-a")), 20)
        .unwrap();
    let namesake = store
        .resolve_or_create_artist(&artist("Renamed Artist", Some("mbid-b")), 30)
        .unwrap();

    assert_eq!(first, renamed);
    assert_ne!(first, namesake);
    let stored = store.artist(first).unwrap().unwrap();
    assert_eq!(stored.mbid.as_deref(), Some("mbid-a"));
    assert_eq!(stored.canonical_name, "Renamed Artist");
}

#[test]
fn an_unambiguous_name_only_artist_is_promoted_but_ambiguous_names_are_not_guessed() {
    let path = database_path("promotion");
    let store = GraphStore::open(&path).unwrap();
    let fallback = store
        .resolve_or_create_artist(&artist("  The   Example  ", None), 10)
        .unwrap();
    let promoted = store
        .resolve_or_create_artist(&artist("the example", Some("mbid-one")), 20)
        .unwrap();
    assert_eq!(fallback, promoted);

    let namesake = store
        .resolve_or_create_artist(&artist("The Example", Some("mbid-two")), 30)
        .unwrap();
    assert_ne!(promoted, namesake);
    assert_eq!(
        store
            .find_artist(&artist("THE EXAMPLE", Some("mbid-one")))
            .unwrap(),
        Some(promoted)
    );
    assert_eq!(
        store.find_artist(&artist("the example", None)).unwrap(),
        None
    );
}

#[test]
fn batch_lookup_uses_mbids_first_and_only_unambiguous_name_fallbacks() {
    let path = database_path("batch");
    let store = GraphStore::open(&path).unwrap();
    let mbid_artist = store
        .resolve_or_create_artist(&artist("Alpha", Some("alpha-mbid")), 10)
        .unwrap();
    let name_artist = store
        .resolve_or_create_artist(&artist("Beta Artist", None), 10)
        .unwrap();

    let found = store
        .find_artists(&[
            artist("Wrong display name", Some("ALPHA-MBID")),
            artist(" beta   artist ", None),
            artist("Missing", None),
        ])
        .unwrap();
    assert_eq!(found, vec![Some(mbid_artist), Some(name_artist), None]);
}

#[test]
fn reverse_edges_exist_only_after_an_independent_observation() {
    let path = database_path("direction");
    let mut store = GraphStore::open(&path).unwrap();
    let a = store
        .resolve_or_create_artist(&artist("A", Some("a")), 10)
        .unwrap();
    let b = store
        .resolve_or_create_artist(&artist("B", Some("b")), 10)
        .unwrap();
    store
        .replace_artist_observation(
            a,
            &observation("a-to-b"),
            &[ArtistEdgeWrite {
                target_artist_id: b,
                rank: 1,
                match_score: Some(0.9),
            }],
        )
        .unwrap();
    assert!(store.artist_observation(b).unwrap().is_none());

    store
        .replace_artist_observation(
            b,
            &observation("b-to-a"),
            &[ArtistEdgeWrite {
                target_artist_id: a,
                rank: 1,
                match_score: Some(0.4),
            }],
        )
        .unwrap();
    assert_eq!(
        store.artist_observation(a).unwrap().unwrap().edges[0].match_score,
        Some(0.9)
    );
    assert_eq!(
        store.artist_observation(b).unwrap().unwrap().edges[0].match_score,
        Some(0.4)
    );
}
