// SPDX-License-Identifier: GPL-3.0-only

use crate::store::{
    ArtistIdentity, ArtistObservationWrite, GraphStore, LegacyArtistObservation,
    LegacyArtistTarget, LegacyGraphImport, LegacyTrackObservation, LegacyTrackTarget,
    ObservationStatus, TrackIdentity,
};
use fs2::FileExt;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::Path;

const CACHE_SCHEMA_VERSION: u8 = 1;
const ARTIST_POSITIVE_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
const EMPTY_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
const TRACK_TTL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    Imported {
        entries: usize,
        artist_observations: usize,
        track_observations: usize,
    },
    AlreadyMigrated,
    NoLegacyCache,
    PreservedInvalid {
        warning: String,
    },
}

#[derive(Debug, Deserialize)]
struct CacheEntry {
    fetched_at_unix_seconds: i64,
    value: Value,
}

#[derive(Debug, Deserialize)]
struct CacheFile {
    schema_version: u8,
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct CachedSimilarArtist {
    name: String,
    mbid: Option<String>,
    score: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
struct CachedSimilarTrack {
    artist: String,
    title: String,
    artist_mbid: Option<String>,
    recording_mbid: Option<String>,
    score: Option<f64>,
}

pub fn migrate_legacy_cache(
    path: impl AsRef<Path>,
    store: &mut GraphStore,
    migrated_at: i64,
) -> Result<MigrationOutcome, String> {
    if store.migrated_from_json_at()?.is_some() {
        return Ok(MigrationOutcome::AlreadyMigrated);
    }
    let path = path.as_ref();
    if !path.exists() {
        return Ok(MigrationOutcome::NoLegacyCache);
    }
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("cannot open legacy Last.fm cache lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock legacy Last.fm cache: {error}"))?;
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read legacy Last.fm cache: {error}"))?;
    let cache = match serde_json::from_slice::<CacheFile>(&bytes) {
        Ok(cache) if cache.schema_version == CACHE_SCHEMA_VERSION => cache,
        Ok(cache) => {
            let warning = format!(
                "unsupported legacy Last.fm cache schema {}; preserving file",
                cache.schema_version
            );
            FileExt::unlock(&lock)
                .map_err(|error| format!("cannot unlock legacy Last.fm cache: {error}"))?;
            return Ok(MigrationOutcome::PreservedInvalid { warning });
        }
        Err(error) => {
            let warning = format!("invalid legacy Last.fm cache; preserving file: {error}");
            FileExt::unlock(&lock)
                .map_err(|unlock| format!("cannot unlock legacy Last.fm cache: {unlock}"))?;
            return Ok(MigrationOutcome::PreservedInvalid { warning });
        }
    };
    let entry_count = cache.entries.len();
    let import = decode_import(cache)?;
    let artist_observations = import.artists.len();
    let track_observations = import.tracks.len();
    store.import_legacy_graph(&import, migrated_at)?;
    fs::remove_file(path)
        .map_err(|error| format!("cannot remove migrated Last.fm cache: {error}"))?;
    FileExt::unlock(&lock)
        .map_err(|error| format!("cannot unlock legacy Last.fm cache: {error}"))?;
    Ok(MigrationOutcome::Imported {
        entries: entry_count,
        artist_observations,
        track_observations,
    })
}

fn decode_import(cache: CacheFile) -> Result<LegacyGraphImport, String> {
    let mut import = LegacyGraphImport::default();
    for (key, entry) in cache.entries {
        if let Some(source_key) = key.strip_prefix("artist:") {
            let targets = serde_json::from_value::<Vec<CachedSimilarArtist>>(entry.value.clone())
                .map_err(|error| {
                format!("cannot decode legacy Last.fm artist entry: {error}")
            })?;
            let status = status(targets.len());
            import.artists.push(LegacyArtistObservation {
                source: artist_source(source_key),
                observation: observation(
                    entry.fetched_at_unix_seconds,
                    &entry.value,
                    targets.len(),
                    status,
                    if targets.is_empty() {
                        EMPTY_TTL_SECONDS
                    } else {
                        ARTIST_POSITIVE_TTL_SECONDS
                    },
                )?,
                targets: targets
                    .into_iter()
                    .enumerate()
                    .map(|(index, target)| LegacyArtistTarget {
                        identity: ArtistIdentity {
                            mbid: target.mbid,
                            name: target.name,
                        },
                        rank: index as u32 + 1,
                        match_score: target.score,
                    })
                    .collect(),
            });
        } else if let Some(source_key) = key.strip_prefix("track:") {
            let targets = serde_json::from_value::<Vec<CachedSimilarTrack>>(entry.value.clone())
                .map_err(|error| format!("cannot decode legacy Last.fm track entry: {error}"))?;
            let status = status(targets.len());
            import.tracks.push(LegacyTrackObservation {
                source: track_source(source_key),
                observation: observation(
                    entry.fetched_at_unix_seconds,
                    &entry.value,
                    targets.len(),
                    status,
                    if targets.is_empty() {
                        EMPTY_TTL_SECONDS
                    } else {
                        TRACK_TTL_SECONDS
                    },
                )?,
                targets: targets
                    .into_iter()
                    .enumerate()
                    .map(|(index, target)| LegacyTrackTarget {
                        identity: TrackIdentity {
                            recording_mbid: target.recording_mbid,
                            artist: ArtistIdentity {
                                mbid: target.artist_mbid,
                                name: target.artist,
                            },
                            title: target.title,
                        },
                        rank: index as u32 + 1,
                        match_score: target.score,
                    })
                    .collect(),
            });
        }
    }
    Ok(import)
}

fn artist_source(key: &str) -> ArtistIdentity {
    if looks_like_mbid(key) {
        ArtistIdentity {
            mbid: Some(key.to_owned()),
            name: key.to_owned(),
        }
    } else {
        ArtistIdentity {
            mbid: None,
            name: key.to_owned(),
        }
    }
}

fn track_source(key: &str) -> TrackIdentity {
    if let Some((artist, title)) = key.split_once('|') {
        TrackIdentity {
            recording_mbid: None,
            artist: ArtistIdentity {
                mbid: None,
                name: artist.to_owned(),
            },
            title: title.to_owned(),
        }
    } else {
        TrackIdentity {
            recording_mbid: Some(key.to_owned()),
            artist: ArtistIdentity {
                mbid: None,
                name: "unknown migrated Last.fm artist".to_owned(),
            },
            title: key.to_owned(),
        }
    }
}

fn observation(
    fetched_at: i64,
    value: &Value,
    result_count: usize,
    result_status: ObservationStatus,
    ttl_seconds: i64,
) -> Result<ArtistObservationWrite, String> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| format!("cannot canonicalize legacy Last.fm cache value: {error}"))?;
    Ok(ArtistObservationWrite {
        fetched_at,
        validated_at: fetched_at,
        changed_at: fetched_at,
        expires_at: fetched_at.saturating_add(ttl_seconds),
        etag: None,
        last_modified: None,
        payload_hash: format!("{:x}", Sha256::digest(payload)),
        acquisition_source: "api_key".to_owned(),
        requested_limit: 25,
        actual_result_count: result_count as u32,
        result_status,
    })
}

fn status(result_count: usize) -> ObservationStatus {
    if result_count == 0 {
        ObservationStatus::Empty
    } else {
        ObservationStatus::Positive
    }
}

fn looks_like_mbid(value: &str) -> bool {
    value.len() == 36
        && value
            .chars()
            .enumerate()
            .all(|(index, character)| match index {
                8 | 13 | 18 | 23 => character == '-',
                _ => character.is_ascii_hexdigit(),
            })
}
