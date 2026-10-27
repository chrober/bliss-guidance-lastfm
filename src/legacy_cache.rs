// SPDX-License-Identifier: GPL-3.0-only

use crate::lastfm_api::SimilarTrack;
use crate::store::{
    ArtistIdentity, ArtistObservationWrite, GraphStore, LegacyArtistObservation,
    LegacyArtistTarget, LegacyGraphImport, ObservationStatus,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

const CACHE_SCHEMA_VERSION: u8 = 1;
const ARTIST_POSITIVE_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
const EMPTY_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
const TRACK_TTL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    Imported {
        entries: usize,
        artist_observations: usize,
        preserved_track_entries: usize,
    },
    AlreadyMigrated,
    NoLegacyCache,
    PreservedInvalid {
        warning: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct CacheEntry {
    fetched_at_unix_seconds: i64,
    value: Value,
}

#[derive(Debug, Deserialize, Serialize)]
struct CacheFile {
    schema_version: u8,
    entries: BTreeMap<String, CacheEntry>,
}

// Count retained value bytes, not JSON keys, punctuation, cache keys, timestamps,
// or pretty-printing. Numbers use their canonical decimal representation.
fn data_value_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(value) => value.to_string().len() as u64,
        Value::String(value) => value.len() as u64,
        Value::Array(values) => values
            .iter()
            .map(data_value_bytes)
            .fold(0, u64::saturating_add),
        Value::Object(values) => values
            .values()
            .map(data_value_bytes)
            .fold(0, u64::saturating_add),
    }
}

fn retained_payload_size(cache: &CacheFile) -> u64 {
    let mut total = 0_u64;
    for entry in cache.entries.values() {
        total = total.saturating_add(data_value_bytes(&entry.value));
    }
    total
}

fn remove_expired_tracks(cache: &mut CacheFile, now: i64) -> bool {
    let before = cache.entries.len();
    cache.entries.retain(|key, entry| {
        !key.starts_with("track:")
            || entry
                .fetched_at_unix_seconds
                .saturating_add(TRACK_TTL_SECONDS)
                > now
    });
    cache.entries.len() != before
}

fn write_cache(path: &Path, cache: &CacheFile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(cache)
        .map_err(|error| format!("cannot encode retained Last.fm cache: {error}"))?;
    let temporary = path.with_extension("track-cache.tmp");
    fs::write(&temporary, bytes)
        .map_err(|error| format!("cannot write Last.fm track cache: {error}"))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("cannot replace Last.fm track cache: {error}"))
}

fn retained_cache_summary(path: &Path, now: i64) -> Result<(u64, Option<i64>), String> {
    if !path.exists() {
        return Ok((0, None));
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .map_err(|e| format!("cannot open Last.fm cache lock: {e}"))?;
    lock.lock_exclusive()
        .map_err(|e| format!("cannot lock Last.fm cache: {e}"))?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, None)),
        Err(error) => return Err(format!("cannot read Last.fm cache: {error}")),
    };
    let mut cache = match serde_json::from_slice::<CacheFile>(&bytes) {
        Ok(value) if value.schema_version == CACHE_SCHEMA_VERSION => value,
        // Do not silently guess a size or overwrite an unreadable cache.
        _ => return Err("cannot account unrecognized Last.fm cache; file preserved".to_owned()),
    };
    if remove_expired_tracks(&mut cache, now) {
        write_cache(path, &cache)?;
    }
    let next_expiry = cache
        .entries
        .iter()
        .filter(|(key, _)| key.starts_with("track:"))
        .map(|(_, value)| {
            value
                .fetched_at_unix_seconds
                .saturating_add(TRACK_TTL_SECONDS)
        })
        .min();
    let size = retained_payload_size(&cache);
    FileExt::unlock(&lock).map_err(|e| format!("cannot unlock Last.fm cache: {e}"))?;
    Ok((size, next_expiry))
}

pub fn retained_cache_bytes(path: impl AsRef<Path>, now: i64) -> Result<u64, String> {
    retained_cache_summary(path.as_ref(), now).map(|value| value.0)
}

#[derive(Default)]
pub struct CacheUsage {
    path: Option<PathBuf>,
    last: Option<(u64, SystemTime, Option<i64>, u64)>,
}

impl CacheUsage {
    pub fn new(path: Option<&str>) -> Self {
        Self {
            path: path.map(PathBuf::from),
            last: None,
        }
    }
    pub fn bytes(&mut self, now: i64) -> Result<u64, String> {
        let Some(path) = self.path.as_deref() else {
            return Ok(0);
        };
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.last = None;
                return Ok(0);
            }
            Err(error) => return Err(format!("cannot inspect Last.fm cache: {error}")),
        };
        if let (Some((length, modified, expiry, bytes)), Ok(current)) =
            (&self.last, metadata.modified())
        {
            if *length == metadata.len()
                && *modified == current
                && expiry.is_none_or(|value| now < value)
            {
                return Ok(*bytes);
            }
        }
        let (bytes, expiry) = retained_cache_summary(path, now)?;
        self.last = match (metadata.modified(), fs::metadata(path)) {
            (Ok(before), Ok(after))
                if metadata.len() == after.len() && after.modified().ok() == Some(before) =>
            {
                Some((after.len(), before, expiry, bytes))
            }
            _ => None,
        };
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct CachedSimilarArtist {
    name: String,
    mbid: Option<String>,
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
    let preserved_entries = cache
        .entries
        .iter()
        .filter(|(key, _)| !key.starts_with("artist:"))
        .map(|(key, entry)| {
            (
                key.clone(),
                CacheEntry {
                    fetched_at_unix_seconds: entry.fetched_at_unix_seconds,
                    value: entry.value.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let preserved_track_entries = preserved_entries
        .keys()
        .filter(|key| key.starts_with("track:"))
        .count();
    let import = decode_import(cache)?;
    let artist_observations = import.artists.len();
    store.import_legacy_graph(&import, migrated_at)?;
    if preserved_entries.is_empty() {
        fs::remove_file(path)
            .map_err(|error| format!("cannot remove migrated Last.fm cache: {error}"))?;
    } else {
        let retained = serde_json::to_vec_pretty(&CacheFile {
            schema_version: CACHE_SCHEMA_VERSION,
            entries: preserved_entries,
        })
        .map_err(|error| format!("cannot encode retained Last.fm track cache: {error}"))?;
        let temporary_path = path.with_extension("migration.tmp");
        fs::write(&temporary_path, retained)
            .map_err(|error| format!("cannot write retained Last.fm track cache: {error}"))?;
        fs::rename(&temporary_path, path)
            .map_err(|error| format!("cannot replace migrated Last.fm cache: {error}"))?;
    }
    FileExt::unlock(&lock)
        .map_err(|error| format!("cannot unlock legacy Last.fm cache: {error}"))?;
    Ok(MigrationOutcome::Imported {
        entries: entry_count,
        artist_observations,
        preserved_track_entries,
    })
}

pub fn cached_tracks(
    path: impl AsRef<Path>,
    key: &str,
    now: i64,
) -> Result<Option<Vec<SimilarTrack>>, String> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read non-persistent Last.fm track cache: {error}"))?;
    let Ok(cache) = serde_json::from_slice::<CacheFile>(&bytes) else {
        return Ok(None);
    };
    if cache.schema_version != CACHE_SCHEMA_VERSION {
        return Ok(None);
    }
    let Some(entry) = cache.entries.get(&format!("track:{key}")) else {
        return Ok(None);
    };
    if entry
        .fetched_at_unix_seconds
        .saturating_add(TRACK_TTL_SECONDS)
        <= now
    {
        return Ok(None);
    }
    serde_json::from_value(entry.value.clone())
        .map(Some)
        .map_err(|error| format!("cannot decode non-persistent Last.fm track cache: {error}"))
}

pub fn cache_tracks(
    path: impl AsRef<Path>,
    key: &str,
    fetched_at: i64,
    tracks: &[SimilarTrack],
) -> Result<(), String> {
    cache_tracks_with_budget(path, key, fetched_at, tracks, u64::MAX).map(|_| ())
}

pub fn cache_tracks_with_budget(
    path: impl AsRef<Path>,
    key: &str,
    fetched_at: i64,
    tracks: &[SimilarTrack],
    budget_bytes: u64,
) -> Result<bool, String> {
    let path = path.as_ref();
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("cannot open Last.fm track-cache lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock Last.fm track cache: {error}"))?;
    let mut cache = if path.exists() {
        let bytes =
            fs::read(path).map_err(|error| format!("cannot read Last.fm track cache: {error}"))?;
        let cache = serde_json::from_slice::<CacheFile>(&bytes)
            .map_err(|error| format!("invalid Last.fm track cache; preserving file: {error}"))?;
        if cache.schema_version != CACHE_SCHEMA_VERSION {
            return Err("unsupported Last.fm track cache; preserving file".to_owned());
        }
        cache
    } else {
        CacheFile {
            schema_version: CACHE_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    };
    let expired_removed = remove_expired_tracks(&mut cache, fetched_at);
    if expired_removed {
        write_cache(path, &cache)?;
    }
    cache.entries.insert(
        format!("track:{key}"),
        CacheEntry {
            fetched_at_unix_seconds: fetched_at,
            value: serde_json::to_value(tracks)
                .map_err(|error| format!("cannot encode Last.fm track cache: {error}"))?,
        },
    );
    if retained_payload_size(&cache) > budget_bytes {
        FileExt::unlock(&lock)
            .map_err(|error| format!("cannot unlock Last.fm track cache: {error}"))?;
        return Ok(false);
    }
    write_cache(path, &cache)?;
    FileExt::unlock(&lock)
        .map_err(|error| format!("cannot unlock Last.fm track cache: {error}"))?;
    Ok(true)
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
