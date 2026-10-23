// SPDX-License-Identifier: GPL-3.0-only

use crate::direct::{persist_artist_response, StorageScope, API_KEY_ENVIRONMENT};
use crate::lastfm_api::{ArtistLimit, LastFmClient, Validators};
use crate::progress::GraphProgress;
use crate::root_snapshot::LibraryArtistSnapshot;
use crate::store::{ArtistIdentity, GraphStore};
use serde::Deserialize;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_BUDGET_BYTES: u64 = 80 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct GraphRequest {
    pub acquisition_mode: Option<String>,
    pub store_path: String,
    pub legacy_cache_path: Option<String>,
    pub roots_path: Option<String>,
    pub api_endpoint: Option<String>,
    pub artist_result_limit: Option<u32>,
    pub cancel_path: Option<String>,
    pub confirmed: Option<bool>,
    pub budget_bytes: Option<u64>,
    pub storage_scope: Option<String>,
}

impl GraphRequest {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read Last.fm graph request: {error}"))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("Last.fm graph request is invalid: {error}"))
    }

    fn require_api_key(&self) -> Result<String, String> {
        if self.acquisition_mode.as_deref() != Some("direct") {
            return Err("Last.fm graph synchronization requires API Key source".to_owned());
        }
        std::env::var(API_KEY_ENVIRONMENT)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "direct Last.fm API key is unavailable".to_owned())
    }

    fn artist_limit(&self) -> Result<ArtistLimit, String> {
        match self.artist_result_limit.unwrap_or(25) {
            25 => Ok(ArtistLimit::TwentyFive),
            50 => Ok(ArtistLimit::Fifty),
            100 => Ok(ArtistLimit::OneHundred),
            _ => Err("Last.fm graph artist result limit is invalid".to_owned()),
        }
    }

    fn storage_scope(&self) -> Result<StorageScope, String> {
        match self
            .storage_scope
            .as_deref()
            .unwrap_or("all_returned_artists")
        {
            "all_returned_artists" => Ok(StorageScope::AllReturnedArtists),
            "local_artists_only" => Ok(StorageScope::LocalArtistsOnly),
            _ => Err("Last.fm graph storage scope is invalid".to_owned()),
        }
    }
}

pub fn run_graph_command(
    command: &str,
    request_path: &Path,
    output: &mut impl Write,
) -> Result<(), String> {
    let request = GraphRequest::load(request_path)?;
    match command {
        "status" => status(&request, output),
        "sync" => sync(&request, output),
        "compact" => compact(&request, output),
        "delete" => delete(&request, output),
        _ => Err(format!("unsupported Last.fm graph command {command}")),
    }
}

fn status(request: &GraphRequest, output: &mut impl Write) -> Result<(), String> {
    if !Path::new(&request.store_path).exists() {
        return emit_json(
            output,
            &json!({"schema_version":1,"state":"missing","database_bytes":0}),
        );
    }
    let store = GraphStore::open(&request.store_path)?;
    let statistics = store.statistics()?;
    emit_json(
        output,
        &json!({
            "schema_version": 1,
            "state": "available",
            "database_bytes": database_bytes(&request.store_path),
            "artists": statistics.artists,
            "artist_observations": statistics.artist_observations,
            "artist_edges": statistics.artist_edges,
            "tracks": statistics.tracks,
            "track_observations": statistics.track_observations,
            "track_edges": statistics.track_edges,
            "library_artists": statistics.library_artists,
            "covered_library_artists": statistics.covered_library_artists,
            "external_artists": statistics.external_artists,
            "reciprocal_artist_pairs": statistics.reciprocal_artist_pairs,
        }),
    )
}

fn sync(request: &GraphRequest, output: &mut impl Write) -> Result<(), String> {
    let api_key = request.require_api_key()?;
    let roots_path = request
        .roots_path
        .as_deref()
        .ok_or_else(|| "Last.fm artist-root snapshot path is missing".to_owned())?;
    let snapshot = LibraryArtistSnapshot::load(roots_path)?;
    let endpoint = request
        .api_endpoint
        .as_deref()
        .unwrap_or("https://ws.audioscrobbler.com/2.0/");
    let limit = request.artist_limit()?;
    let storage_scope = request.storage_scope()?;
    let now = unix_seconds();
    let mut store = GraphStore::open_at(&request.store_path, now)?;
    let mut artist_ids = Vec::with_capacity(snapshot.artists.len());
    for root in &snapshot.artists {
        artist_ids.push(store.resolve_or_create_artist(
            &ArtistIdentity {
                mbid: root.artist_mbids.first().cloned(),
                name: root.name.clone(),
            },
            now,
        )?);
    }
    store.replace_library_membership(
        &snapshot.snapshot_id,
        &snapshot.library_identity,
        snapshot.generated_at,
        &artist_ids,
    )?;

    let total = snapshot.artists.len() as u64;
    let mut progress = GraphProgress::new("sync", "Updating offline Last.fm artist graph");
    progress.total = total;
    progress.database_bytes = database_bytes(&request.store_path);
    emit_json(output, &progress)?;
    let client = LastFmClient::new(endpoint, &api_key, Duration::from_secs(30))?;
    for (index, root) in snapshot.artists.iter().enumerate() {
        if request
            .cancel_path
            .as_deref()
            .is_some_and(|path| Path::new(path).exists())
        {
            progress.cancelled = true;
            progress.finished = true;
            progress.message = "Last.fm graph update cancelled".to_owned();
            return emit_json(output, &progress);
        }
        let identity = ArtistIdentity {
            mbid: root.artist_mbids.first().cloned(),
            name: root.name.clone(),
        };
        let source_id = store.resolve_or_create_artist(&identity, now)?;
        let stored = store.artist_observation(source_id)?;
        let sufficient = stored
            .as_ref()
            .is_some_and(|value| value.expires_at > now && value.requested_limit >= limit.value());
        if sufficient {
            progress.cached += 1;
        } else {
            if total_storage_bytes(request) >= request.budget_bytes.unwrap_or(DEFAULT_BUDGET_BYTES)
            {
                progress.budget_exhausted = true;
                progress.finished = true;
                progress.message = "Last.fm graph storage budget is exhausted".to_owned();
                emit_json(output, &progress)?;
                return Ok(());
            }
            let validators = stored.as_ref().map(|value| Validators {
                etag: value.etag.clone(),
                last_modified: value.last_modified.clone(),
            });
            let mut attempt = 0_u32;
            let response = loop {
                progress.requests += 1;
                match client.similar_artists(
                    &root.name,
                    root.artist_mbids.first().map(String::as_str),
                    limit,
                    validators.as_ref(),
                ) {
                    Ok(response) => break response,
                    Err(error) if error.is_retryable() && attempt < 2 => {
                        let delay = 1_u64 << attempt;
                        if error
                            .retry_after_seconds()
                            .is_some_and(|value| value > delay)
                        {
                            progress.failures += 1;
                            progress.paused = true;
                            progress.finished = true;
                            progress.retry_at = Some(
                                unix_seconds() + error.retry_after_seconds().unwrap_or(60) as i64,
                            );
                            progress.message =
                                "Last.fm graph update paused after rate limiting".to_owned();
                            emit_json(output, &progress)?;
                            return Ok(());
                        }
                        std::thread::sleep(Duration::from_secs(delay));
                        attempt += 1;
                    }
                    Err(error) if error.is_retryable() => {
                        progress.failures += 1;
                        progress.paused = true;
                        progress.finished = true;
                        progress.retry_at =
                            Some(unix_seconds() + error.retry_after_seconds().unwrap_or(60) as i64);
                        progress.message =
                            "Last.fm graph update paused after a temporary request failure"
                                .to_owned();
                        emit_json(output, &progress)?;
                        return Ok(());
                    }
                    Err(error) => {
                        progress.failures += 1;
                        return Err(error.message().to_owned());
                    }
                }
            };
            if persist_artist_response(
                &mut store,
                &identity,
                &response,
                unix_seconds(),
                storage_scope,
            )? {
                progress.changed += 1;
            }
            if total_storage_bytes(request) >= request.budget_bytes.unwrap_or(DEFAULT_BUDGET_BYTES)
            {
                progress.budget_exhausted = true;
                progress.finished = true;
                progress.database_bytes = database_bytes(&request.store_path);
                progress.message =
                    "Last.fm graph storage budget was reached after the current artist".to_owned();
                emit_json(output, &progress)?;
                return Ok(());
            }
            if index + 1 < snapshot.artists.len() {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        progress.current = index as u64 + 1;
        progress.database_bytes = database_bytes(&request.store_path);
        progress.message = format!("Updated {}/{} library artists", progress.current, total);
        emit_json(output, &progress)?;
    }
    progress.finished = true;
    progress.message = "Last.fm graph update completed".to_owned();
    emit_json(output, &progress)
}

fn compact(request: &GraphRequest, output: &mut impl Write) -> Result<(), String> {
    request.require_api_key()?;
    let mut store = GraphStore::open(&request.store_path)?;
    let limit = request.artist_limit()?.value();
    let local_only = request.storage_scope()? == StorageScope::LocalArtistsOnly;
    store.checkpoint_and_compact(limit, local_only)?;
    let statistics = store.statistics()?;
    emit_json(
        output,
        &json!({
            "schema_version":1,
            "state":"compacted",
            "database_bytes":database_bytes(&request.store_path),
            "artist_edges":statistics.artist_edges,
        }),
    )
}

fn delete(request: &GraphRequest, output: &mut impl Write) -> Result<(), String> {
    if request.confirmed != Some(true) {
        return Err("Last.fm graph deletion requires explicit confirmation".to_owned());
    }
    for path in database_paths(&request.store_path) {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("cannot delete Last.fm graph file: {error}"))?;
        }
    }
    if let Some(path) = request.legacy_cache_path.as_deref().map(PathBuf::from) {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("cannot delete legacy Last.fm cache: {error}"))?;
        }
    }
    emit_json(output, &json!({"schema_version":1,"state":"deleted"}))
}

fn emit_json(output: &mut impl Write, value: &impl serde::Serialize) -> Result<(), String> {
    serde_json::to_writer(&mut *output, value)
        .map_err(|error| format!("cannot encode Last.fm graph output: {error}"))?;
    writeln!(output).map_err(|error| format!("cannot write Last.fm graph output: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("cannot flush Last.fm graph output: {error}"))
}

fn database_paths(store_path: &str) -> Vec<PathBuf> {
    vec![
        PathBuf::from(store_path),
        PathBuf::from(format!("{store_path}-wal")),
        PathBuf::from(format!("{store_path}-shm")),
    ]
}

fn database_bytes(store_path: &str) -> u64 {
    database_paths(store_path)
        .into_iter()
        .filter_map(|path| std::fs::metadata(path).ok().map(|value| value.len()))
        .sum()
}

fn total_storage_bytes(request: &GraphRequest) -> u64 {
    database_bytes(&request.store_path)
        + request
            .legacy_cache_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok().map(|value| value.len()))
            .unwrap_or(0)
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
