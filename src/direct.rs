use crate::freshness::{
    artist_observation_is_fresh, canonical_artist_payload_hash, expires_at, FreshnessClass,
};
use crate::lastfm_api::{
    ArtistLimit, LastFmClient, LastFmError, ResponseEnvelope, ResponseStatus, SimilarArtist,
    SimilarTrack, Validators,
};
use crate::store::{
    ArtistEdgeWrite, ArtistIdentity, ArtistObservationRecord, ArtistObservationWrite, GraphStore,
    ObservationStatus, ObservationValidation,
};
use bliss_playlist_guidance_spi::{Anchor, Candidate, Diagnostics, ScoreContext};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const API_KEY_ENVIRONMENT: &str = "BLISS_GUIDANCE_LASTFM_API_KEY";
const MAX_TRANSIENT_RETRIES: u8 = 2;
const RETRY_BACKOFF_MILLIS: u64 = 100;
const MAX_LOG_EVENTS: usize = 64;

fn wants_info(level: &str) -> bool {
    matches!(level, "info" | "debug")
}

fn wants_debug(level: &str) -> bool {
    level == "debug"
}

// The remaining parameters mirror the optional fields of guidance_log_event_v1.
#[allow(clippy::too_many_arguments)]
fn push_log_event(
    events: &mut Vec<Value>,
    level: &str,
    event: &str,
    operation: Option<&str>,
    source: Option<Value>,
    cache_state: Option<&str>,
    result_count: Option<usize>,
    details: Option<Value>,
) {
    if events.len() >= MAX_LOG_EVENTS {
        if matches!(level, "warning" | "error") {
            if let Some(index) = events
                .iter()
                .position(|value| value["level"] == "debug")
                .or_else(|| events.iter().position(|value| value["level"] == "info"))
            {
                events.remove(index);
            } else {
                return;
            }
        } else {
            return;
        }
    }
    events.push(serde_json::json!({
        "event_version": "guidance_log_event_v1",
        "level": level,
        "phase": "prepare",
        "event": event,
        "provider_id": "lastfm-guidance",
        "operation": operation,
        "source": source,
        "cache_state": cache_state,
        "result_count": result_count,
        "details": details,
    }));
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectOptions {
    pub api_endpoint: String,
    pub store_path: String,
    pub library_db_path: Option<String>,
    pub legacy_cache_path: Option<String>,
    pub retrieval_policy: RetrievalPolicy,
    pub artist_result_limit: ArtistLimit,
    pub storage_scope: StorageScope,
    pub request_deadline_ms: u64,
    pub max_concurrent_requests: usize,
    pub budget_bytes: u64,
    pub artist_refresh_days: Option<u32>,
    pub empty_artist_refresh_days: Option<u32>,
    pub diagnostic_level: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetrievalPolicy {
    OnlineWithCache,
    OfflinePreferred,
    OfflineOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageScope {
    AllReturnedArtists,
    LocalArtistsOnly,
}

impl DirectOptions {
    pub fn from_prepare_options(options: &Value) -> Result<Self, String> {
        let object = options
            .as_object()
            .ok_or_else(|| "direct Last.fm options must be an object".to_owned())?;
        if object.get("acquisition_mode").and_then(Value::as_str) != Some("direct") {
            return Err("direct Last.fm acquisition mode is required".to_owned());
        }
        let store_path = object
            .get("store_path")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "trusted Last.fm graph path is required".to_owned())?;
        let api_endpoint = object
            .get("api_endpoint")
            .and_then(Value::as_str)
            .unwrap_or("https://ws.audioscrobbler.com/2.0/")
            .trim_end_matches('/')
            .to_owned();
        if !(api_endpoint.starts_with("https://") || api_endpoint.starts_with("http://")) {
            return Err("trusted Last.fm endpoint is invalid".to_owned());
        }
        let retrieval_policy = match object
            .get("retrieval_policy")
            .and_then(Value::as_str)
            .unwrap_or("online_with_cache")
        {
            "online_with_cache" => RetrievalPolicy::OnlineWithCache,
            "offline_preferred" => RetrievalPolicy::OfflinePreferred,
            "offline_only" => RetrievalPolicy::OfflineOnly,
            _ => return Err("direct Last.fm retrieval policy is invalid".to_owned()),
        };
        let artist_result_limit = object
            .get("artist_result_limit")
            .and_then(Value::as_u64)
            .unwrap_or(25)
            .try_into()
            .map(ArtistLimit::from_value)
            .map_err(|_| "direct Last.fm artist result limit is invalid".to_owned())?;
        let storage_scope = match object
            .get("storage_scope")
            .and_then(Value::as_str)
            .unwrap_or("all_returned_artists")
        {
            "all_returned_artists" => StorageScope::AllReturnedArtists,
            "local_artists_only" => StorageScope::LocalArtistsOnly,
            _ => return Err("direct Last.fm storage scope is invalid".to_owned()),
        };
        let request_deadline_ms = integer_option(object, "request_deadline_ms", 100, 60_000)?;
        let max_concurrent_requests = integer_option(object, "max_concurrent_requests", 1, 8)?;
        let budget_bytes = object
            .get("budget_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(100_000_000);
        let diagnostic_level = object
            .get("diagnostic_level")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "warning" | "info" | "debug"))
            .unwrap_or("warning")
            .to_owned();
        Ok(Self {
            api_endpoint,
            store_path: store_path.to_owned(),
            library_db_path: object
                .get("library_db_path")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            legacy_cache_path: object
                .get("legacy_cache_path")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            retrieval_policy,
            artist_result_limit,
            storage_scope,
            request_deadline_ms,
            max_concurrent_requests: max_concurrent_requests as usize,
            budget_bytes,
            artist_refresh_days: optional_refresh_days(object, "artist_refresh_days")?,
            empty_artist_refresh_days: optional_refresh_days(object, "empty_artist_refresh_days")?,
            diagnostic_level,
        })
    }

    pub fn api_key_from_environment(&self) -> Result<String, String> {
        std::env::var(API_KEY_ENVIRONMENT)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "direct Last.fm API key is unavailable".to_owned())
    }
}

#[derive(Clone, Debug)]
pub struct DirectPrepared {
    relations_by_source: HashMap<String, Vec<DirectRelation>>,
    pub request_count: u64,
    pub cache_hits: u64,
    pub failure_count: u64,
}

#[derive(Clone, Debug)]
struct DirectRelation {
    channel: &'static str,
    score: f64,
    track: Option<SimilarTrack>,
    artist: Option<SimilarArtist>,
}

#[derive(Clone, Debug)]
enum FetchTask {
    Track {
        key: String,
        artist: String,
        title: String,
        recording_mbid: Option<String>,
    },
    Artist {
        key: String,
        source_id: i64,
        artist: String,
        artist_mbid: Option<String>,
        existing: Option<ArtistObservationRecord>,
    },
}

#[derive(Clone, Debug)]
enum FetchResponse {
    Track(ResponseEnvelope<SimilarTrack>),
    Artist(ResponseEnvelope<SimilarArtist>),
}

impl FetchTask {
    fn key(&self) -> &str {
        match self {
            Self::Track { key, .. } | Self::Artist { key, .. } => key,
        }
    }

    fn validators(&self) -> Option<Validators> {
        match self {
            Self::Track { .. } => None,
            Self::Artist { existing, .. } => existing.as_ref().map(|value| Validators {
                etag: value.etag.clone(),
                last_modified: value.last_modified.clone(),
            }),
        }
    }

    fn operation(&self) -> &'static str {
        match self {
            Self::Track { .. } => "track.getSimilar",
            Self::Artist { .. } => "artist.getSimilar",
        }
    }

    fn source(&self) -> Value {
        match self {
            Self::Track {
                artist,
                title,
                recording_mbid,
                ..
            } => serde_json::json!({
                "label": format!("{artist} - {title}"),
                "identity_kind": if recording_mbid.is_some() { "recording_mbid" } else { "track_name" },
                "mbid": recording_mbid,
                "artist": artist,
                "title": title,
            }),
            Self::Artist {
                artist,
                artist_mbid,
                ..
            } => serde_json::json!({
                "label": artist,
                "identity_kind": if artist_mbid.is_some() { "artist_mbid" } else { "artist_name" },
                "mbid": artist_mbid,
            }),
        }
    }

    fn sanitized_request(&self) -> Value {
        match self {
            Self::Track {
                artist,
                title,
                recording_mbid,
                ..
            } => serde_json::json!({
                "method": self.operation(), "artist": artist, "track": title,
                "mbid": recording_mbid, "limit": 25,
            }),
            Self::Artist {
                artist,
                artist_mbid,
                ..
            } => serde_json::json!({
                "method": self.operation(), "artist": artist,
                "mbid": artist_mbid, "limit": 25,
            }),
        }
    }

    fn fetch(
        &self,
        endpoint: &str,
        api_key: &str,
        deadline: Instant,
        artist_limit: ArtistLimit,
    ) -> Result<FetchResponse, LastFmError> {
        let timeout = remaining_duration(deadline)
            .map_err(|_| LastFmError::retryable("direct Last.fm prepare deadline exceeded"))?;
        let client = LastFmClient::new(endpoint, api_key, timeout)
            .map_err(|_| LastFmError::permanent("direct Last.fm configuration is invalid"))?;
        let validators = self.validators();
        match self {
            Self::Track {
                artist,
                title,
                recording_mbid,
                ..
            } => client
                .similar_tracks(
                    artist,
                    title,
                    recording_mbid.as_deref(),
                    validators.as_ref(),
                )
                .map(FetchResponse::Track),
            Self::Artist {
                artist,
                artist_mbid,
                ..
            } => client
                .similar_artists(
                    artist,
                    artist_mbid.as_deref(),
                    artist_limit,
                    validators.as_ref(),
                )
                .map(FetchResponse::Artist),
        }
    }
}

impl DirectPrepared {
    pub fn prepare(
        options: &DirectOptions,
        anchors: &[Anchor],
    ) -> Result<(Self, Diagnostics), String> {
        let api_key = options.api_key_from_environment()?;
        let now = unix_seconds() as i64;
        let deadline = Instant::now() + Duration::from_millis(options.request_deadline_ms);
        let mut store = GraphStore::open_at(&options.store_path, now)?;
        if let Some(path) = options.legacy_cache_path.as_deref() {
            let _ = crate::legacy_cache::migrate_legacy_cache(path, &mut store, now)?;
        }
        let mut cache_usage =
            crate::legacy_cache::CacheUsage::new(options.legacy_cache_path.as_deref());
        let _ = cache_usage.bytes(now)?;
        let mut track_queries = BTreeMap::<String, (String, String, Option<String>)>::new();
        let mut artist_queries = BTreeMap::<String, (String, Option<String>)>::new();
        for anchor in anchors {
            let track = &anchor.track;
            if let (Some(artist), Some(title)) = (track.artist.as_deref(), track.title.as_deref()) {
                track_queries
                    .entry(track_key(artist, title, track.recording_mbid.as_deref()))
                    .or_insert_with(|| {
                        (
                            artist.to_owned(),
                            title.to_owned(),
                            track.recording_mbid.clone(),
                        )
                    });
            }
            if let Some(artist) = track.artist.as_deref() {
                artist_queries
                    .entry(artist_key(
                        artist,
                        track.artist_mbids.first().map(String::as_str),
                    ))
                    .or_insert_with(|| (artist.to_owned(), track.artist_mbids.first().cloned()));
            }
        }

        let mut log_events = Vec::new();
        if wants_info(&options.diagnostic_level) {
            push_log_event(
                &mut log_events,
                "info",
                "lastfm_prepare_started",
                None,
                None,
                None,
                None,
                Some(serde_json::json!({
                    "source": "API Key",
                    "anchors": anchors.len(),
                    "track_lookups": track_queries.len(),
                    "artist_lookups": artist_queries.len(),
                })),
            );
        }
        let mut request_count = 0_u64;
        let mut cache_hits = 0_u64;
        let mut stale_offline = 0_u64;
        let mut missing = 0_u64;
        let mut persistence_failed = 0_u64;
        let mut tracks = HashMap::<String, Vec<SimilarTrack>>::new();
        let mut misses = Vec::new();
        for (key, (artist, title, mbid)) in &track_queries {
            let task = FetchTask::Track {
                key: key.clone(),
                artist: artist.clone(),
                title: title.clone(),
                recording_mbid: mbid.clone(),
            };
            let cached = options
                .legacy_cache_path
                .as_deref()
                .map(|path| crate::legacy_cache::cached_tracks(path, key, now))
                .transpose()?
                .flatten();
            if let Some(value) = cached {
                cache_hits += 1;
                let result_count = value.len();
                if wants_info(&options.diagnostic_level) {
                    push_log_event(
                        &mut log_events,
                        "info",
                        "lastfm_lookup_completed",
                        Some(task.operation()),
                        Some(task.source()),
                        Some("hit"),
                        Some(result_count),
                        None,
                    );
                }
                tracks.insert(key.clone(), value);
            } else if options.retrieval_policy == RetrievalPolicy::OfflineOnly {
                missing += 1;
            } else {
                if wants_info(&options.diagnostic_level) {
                    push_log_event(
                        &mut log_events,
                        "info",
                        "lastfm_lookup_requested",
                        Some(task.operation()),
                        Some(task.source()),
                        Some("miss"),
                        None,
                        None,
                    );
                }
                if wants_debug(&options.diagnostic_level) {
                    push_log_event(
                        &mut log_events,
                        "debug",
                        "lastfm_request_json",
                        Some(task.operation()),
                        Some(task.source()),
                        Some("miss"),
                        None,
                        Some(serde_json::json!({"request": task.sanitized_request()})),
                    );
                }
                misses.push(task);
            }
        }
        let mut artists = HashMap::<String, Vec<SimilarArtist>>::new();
        for (key, (artist, mbid)) in &artist_queries {
            let source_id = store.resolve_or_create_artist(
                &ArtistIdentity {
                    mbid: mbid.clone(),
                    name: artist.clone(),
                },
                now,
            )?;
            let existing = store.artist_observation(source_id)?;
            let task = FetchTask::Artist {
                key: key.clone(),
                source_id,
                artist: artist.clone(),
                artist_mbid: mbid.clone(),
                existing: existing.clone(),
            };
            let complete = existing.as_ref().is_some_and(|value| {
                options
                    .artist_result_limit
                    .is_satisfied_by(value.requested_limit)
            });
            let fresh = existing.as_ref().is_some_and(|value| {
                artist_observation_is_fresh(
                    now,
                    value.validated_at,
                    value.expires_at,
                    if value.result_status == ObservationStatus::Empty {
                        options.empty_artist_refresh_days
                    } else {
                        options.artist_refresh_days
                    },
                )
            }) && complete;
            let use_stored = fresh
                || matches!(
                    options.retrieval_policy,
                    RetrievalPolicy::OfflinePreferred | RetrievalPolicy::OfflineOnly
                ) && existing.is_some();
            if use_stored {
                cache_hits += 1;
                if !fresh {
                    stale_offline += 1;
                }
                let value = stored_artists(
                    &store,
                    existing.as_ref().unwrap(),
                    options.artist_result_limit.value(),
                )?;
                let result_count = value.len();
                if wants_info(&options.diagnostic_level) {
                    push_log_event(
                        &mut log_events,
                        "info",
                        "lastfm_lookup_completed",
                        Some(task.operation()),
                        Some(task.source()),
                        Some(if fresh { "hit" } else { "stale_offline" }),
                        Some(result_count),
                        None,
                    );
                }
                if wants_debug(&options.diagnostic_level) {
                    push_log_event(
                        &mut log_events,
                        "debug",
                        "lastfm_response_json",
                        Some(task.operation()),
                        Some(task.source()),
                        Some("hit"),
                        Some(result_count),
                        Some(serde_json::json!({"results": value})),
                    );
                }
                artists.insert(key.clone(), value);
                if !fresh && options.retrieval_policy == RetrievalPolicy::OfflinePreferred {
                    misses.push(task);
                }
            } else if options.retrieval_policy == RetrievalPolicy::OfflineOnly {
                missing += 1;
            } else {
                if wants_info(&options.diagnostic_level) {
                    push_log_event(
                        &mut log_events,
                        "info",
                        "lastfm_lookup_requested",
                        Some(task.operation()),
                        Some(task.source()),
                        Some("miss"),
                        None,
                        None,
                    );
                }
                if wants_debug(&options.diagnostic_level) {
                    push_log_event(
                        &mut log_events,
                        "debug",
                        "lastfm_request_json",
                        Some(task.operation()),
                        Some(task.source()),
                        Some("miss"),
                        None,
                        Some(serde_json::json!({"request": task.sanitized_request()})),
                    );
                }
                misses.push(task);
            }
        }

        let FetchBatch {
            values,
            request_count: batch_request_count,
            failure_count: failed_requests,
            failures,
        } = fetch_concurrently(options, &api_key, deadline, misses);
        request_count += batch_request_count;
        let mut membership_ready: Option<bool> = None;
        for (task, response) in values {
            let key = task.key().to_owned();
            let result_count = match &response {
                FetchResponse::Track(value) => value.actual_result_count as usize,
                FetchResponse::Artist(value) => value.actual_result_count as usize,
            };
            if wants_info(&options.diagnostic_level) {
                push_log_event(
                    &mut log_events,
                    "info",
                    "lastfm_lookup_completed",
                    Some(task.operation()),
                    Some(task.source()),
                    Some("miss"),
                    Some(result_count),
                    None,
                );
            }
            if wants_debug(&options.diagnostic_level) {
                let debug_results = match &response {
                    FetchResponse::Track(value) => serde_json::json!(value.results),
                    FetchResponse::Artist(value) => serde_json::json!(value.results),
                };
                push_log_event(
                    &mut log_events,
                    "debug",
                    "lastfm_response_json",
                    Some(task.operation()),
                    Some(task.source()),
                    Some("miss"),
                    Some(result_count),
                    Some(serde_json::json!({"results": debug_results})),
                );
            }
            if matches!(response, FetchResponse::Artist(_)) {
                let may_persist = if options.storage_scope == StorageScope::LocalArtistsOnly {
                    *membership_ready.get_or_insert_with(|| {
                        options.library_db_path.as_deref().is_some_and(|path| {
                            crate::library_membership::refresh(
                                std::path::Path::new(path),
                                std::path::Path::new(&options.store_path),
                            )
                            .is_ok()
                        })
                    })
                } else {
                    true
                };
                if !may_persist {
                    persistence_failed += 1;
                } else {
                    match persist_response(
                        &mut store,
                        &task,
                        &response,
                        now,
                        options.storage_scope,
                        Some(
                            options
                                .budget_bytes
                                .saturating_sub(cache_usage.bytes(unix_seconds() as i64)?),
                        ),
                    ) {
                        Err(_) => persistence_failed += 1,
                        Ok(true) => {}
                        Ok(false) if wants_info(&options.diagnostic_level) => {
                            push_log_event(
                                &mut log_events,
                                "info",
                                "lastfm_persistence_skipped",
                                Some(task.operation()),
                                Some(task.source()),
                                Some("budget_exhausted"),
                                Some(result_count),
                                Some(serde_json::json!({"budget_bytes": options.budget_bytes})),
                            );
                        }
                        Ok(false) => {}
                    }
                }
            }
            match response {
                FetchResponse::Track(value) => {
                    if let Some(path) = options.legacy_cache_path.as_deref() {
                        let graph_bytes = store.retained_lastfm_bytes()?;
                        match crate::legacy_cache::cache_tracks_with_budget(
                            path,
                            &key,
                            now,
                            &value.results,
                            options.budget_bytes.saturating_sub(graph_bytes),
                        ) {
                            Err(_) => persistence_failed += 1,
                            Ok(false) if wants_info(&options.diagnostic_level) => push_log_event(
                                &mut log_events,
                                "info",
                                "lastfm_persistence_skipped",
                                Some(task.operation()),
                                Some(task.source()),
                                Some("budget_exhausted"),
                                Some(result_count),
                                Some(serde_json::json!({"budget_bytes": options.budget_bytes})),
                            ),
                            _ => {}
                        }
                    }
                    tracks.insert(key, value.results);
                }
                FetchResponse::Artist(value) => {
                    if value.status == ResponseStatus::Modified {
                        artists.insert(key, value.results);
                    } else if let Some(existing) = task_artist_observation(&task) {
                        artists.insert(
                            key,
                            stored_artists(&store, existing, options.artist_result_limit.value())?,
                        );
                    }
                }
            }
        }
        for (task, error) in failures {
            push_log_event(
                &mut log_events,
                "warning",
                "lastfm_lookup_failed",
                Some(task.operation()),
                Some(task.source()),
                Some("miss"),
                None,
                Some(serde_json::json!({"error": error})),
            );
        }
        if !artist_queries.is_empty() && options.storage_scope != StorageScope::LocalArtistsOnly {
            if let Some(library) = options.library_db_path.as_deref() {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe)
                        .arg("refresh-library-membership")
                        .arg(library)
                        .arg(&options.store_path)
                        .env_remove(API_KEY_ENVIRONMENT)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
            }
        }
        let mut relations_by_source = HashMap::new();
        for anchor in anchors {
            let mut relations = Vec::new();
            let track = &anchor.track;
            if let (Some(artist), Some(title)) = (track.artist.as_deref(), track.title.as_deref()) {
                if let Some(related) =
                    tracks.get(&track_key(artist, title, track.recording_mbid.as_deref()))
                {
                    relations.extend(related.iter().cloned().map(|track| DirectRelation {
                        channel: "lastfm_track",
                        score: track.score.unwrap_or(0.0),
                        track: Some(track),
                        artist: None,
                    }));
                }
            }
            if let Some(artist) = track.artist.as_deref() {
                if let Some(related) = artists.get(&artist_key(
                    artist,
                    track.artist_mbids.first().map(String::as_str),
                )) {
                    relations.extend(related.iter().cloned().map(|artist| DirectRelation {
                        channel: "lastfm_artist",
                        score: artist.score.unwrap_or(0.0),
                        track: None,
                        artist: Some(artist),
                    }));
                }
            }
            if !relations.is_empty() {
                relations_by_source.insert(anchor.anchor_id.clone(), relations);
            }
        }
        let prepared = Self {
            relations_by_source,
            request_count,
            cache_hits,
            failure_count: failed_requests,
        };
        let relation_count = prepared
            .relations_by_source
            .values()
            .map(Vec::len)
            .sum::<usize>();
        let failure_count = prepared.failure_count;
        let state = if persistence_failed > 0 {
            "persistence_failed"
        } else if failure_count == 0 && missing == 0 && stale_offline == 0 {
            "fresh"
        } else if stale_offline > 0 {
            "stale_offline"
        } else if missing > 0 {
            "missing"
        } else {
            "partial"
        };
        if wants_info(&options.diagnostic_level) {
            push_log_event(
                &mut log_events,
                "info",
                "lastfm_prepare_completed",
                None,
                None,
                None,
                Some(relation_count),
                Some(serde_json::json!({
                    "requests": request_count,
                    "cache_hits": cache_hits,
                    "failures": failure_count,
                    "relations": relation_count,
                })),
            );
        }
        Ok((
            prepared,
            Diagnostics {
                state: Some(state.to_owned()),
                request_count,
                failure_count,
                details: Some(serde_json::json!({
                    "acquisition_mode": "direct", "cache_hits": cache_hits,
                    "source_anchors": anchors.len(), "relations": relation_count,
                    "failed_requests": failure_count,
                    "fresh": request_count,
                    "cached": cache_hits,
                    "stale_offline": stale_offline,
                    "missing": missing,
                    "persistence_failed": persistence_failed,
                    "log_events_v1": log_events,
                })),
            },
        ))
    }

    pub fn signals(
        &self,
        context: &ScoreContext,
        candidates: &[Candidate],
    ) -> Vec<(String, String, f64)> {
        let mut source_ids = context.context_track_ids.clone();
        source_ids.extend(
            [
                context.left_anchor_id.as_deref(),
                context.right_anchor_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(str::to_owned),
        );
        source_ids.sort_unstable();
        source_ids.dedup();
        let mut best = HashMap::<(String, String), f64>::new();
        for source_id in source_ids {
            for relation in self
                .relations_by_source
                .get(&source_id)
                .into_iter()
                .flatten()
            {
                for candidate in candidates {
                    if relation.matches(candidate) {
                        let key = (candidate.candidate_id.clone(), relation.channel.to_owned());
                        let entry = best.entry(key).or_insert(0.0);
                        *entry = entry.max(relation.score.clamp(0.0, 1.0));
                    }
                }
            }
        }
        let mut signals = best
            .into_iter()
            .map(|((candidate_id, channel), score)| (candidate_id, channel, score))
            .collect::<Vec<_>>();
        signals.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        signals
    }
}

fn task_artist_observation(task: &FetchTask) -> Option<&ArtistObservationRecord> {
    match task {
        FetchTask::Artist { existing, .. } => existing.as_ref(),
        FetchTask::Track { .. } => None,
    }
}

fn stored_artists(
    store: &GraphStore,
    observation: &ArtistObservationRecord,
    limit: u32,
) -> Result<Vec<SimilarArtist>, String> {
    let take = if limit == 0 {
        usize::MAX
    } else {
        limit as usize
    };
    observation
        .edges
        .iter()
        .take(take)
        .map(|edge| {
            let target = store
                .artist(edge.target_artist_id)?
                .ok_or_else(|| "Last.fm artist edge target is missing".to_owned())?;
            Ok(SimilarArtist {
                name: target.canonical_name,
                mbid: target.mbid,
                score: edge.match_score,
            })
        })
        .collect()
}

fn persist_response(
    store: &mut GraphStore,
    task: &FetchTask,
    response: &FetchResponse,
    now: i64,
    storage_scope: StorageScope,
    budget_bytes: Option<u64>,
) -> Result<bool, String> {
    match (task, response) {
        (
            FetchTask::Artist {
                source_id,
                key,
                existing,
                ..
            },
            FetchResponse::Artist(response),
        ) => {
            let empty = if response.status == ResponseStatus::NotModified {
                existing
                    .as_ref()
                    .is_some_and(|value| value.result_status == ObservationStatus::Empty)
            } else {
                response.results.is_empty()
            };
            let class = if empty {
                FreshnessClass::ArtistEmpty
            } else {
                FreshnessClass::ArtistPositive
            };
            let expiry = expires_at(
                now,
                response.headers.cache_control.as_deref(),
                response.headers.expires.as_deref(),
                class,
            );
            if response.status == ResponseStatus::NotModified {
                store.revalidate_artist_observation(
                    *source_id,
                    &validation(response, None, now, expiry),
                )?;
                return Ok(true);
            }
            let hash_rows = response
                .results
                .iter()
                .enumerate()
                .map(|(index, target)| {
                    (
                        target.mbid.as_deref().unwrap_or(""),
                        target.name.as_str(),
                        index as u32 + 1,
                        target.score,
                    )
                })
                .collect::<Vec<_>>();
            let hash = canonical_artist_payload_hash("artist.getSimilar", key, &hash_rows);
            if existing
                .as_ref()
                .is_some_and(|value| value.payload_hash == hash)
            {
                store.revalidate_artist_observation(
                    *source_id,
                    &validation(response, Some(response.requested_limit), now, expiry),
                )?;
                return Ok(true);
            }
            let mut edges = Vec::with_capacity(response.results.len());
            for (index, target) in response.results.iter().enumerate() {
                let identity = ArtistIdentity {
                    mbid: target.mbid.clone(),
                    name: target.name.clone(),
                };
                let target_id = match storage_scope {
                    StorageScope::AllReturnedArtists => {
                        store.resolve_or_create_artist(&identity, now)?
                    }
                    StorageScope::LocalArtistsOnly => {
                        let Some(target_id) = store.find_artist(&identity)? else {
                            continue;
                        };
                        if !store.is_library_artist(target_id)? {
                            continue;
                        }
                        target_id
                    }
                };
                edges.push(ArtistEdgeWrite {
                    target_artist_id: target_id,
                    rank: index as u32 + 1,
                    match_score: target.score,
                });
            }
            if let Some(budget_bytes) = budget_bytes {
                store.replace_artist_observation_with_budget(
                    *source_id,
                    &observation_write(response, hash, now, expiry),
                    &edges,
                    budget_bytes,
                )
            } else {
                store.replace_artist_observation(
                    *source_id,
                    &observation_write(response, hash, now, expiry),
                    &edges,
                )?;
                Ok(true)
            }
        }
        (FetchTask::Track { .. }, FetchResponse::Track(_)) => Ok(true),
        _ => Err("Last.fm fetch task and response do not match".to_owned()),
    }
}

pub(crate) fn persist_artist_response(
    store: &mut GraphStore,
    source: &ArtistIdentity,
    response: &ResponseEnvelope<SimilarArtist>,
    now: i64,
    storage_scope: StorageScope,
    budget_bytes: u64,
) -> Result<Option<bool>, String> {
    let source_id = store.resolve_or_create_artist(source, now)?;
    let existing = store.artist_observation(source_id)?;
    let old_hash = existing.as_ref().map(|value| value.payload_hash.clone());
    let task = FetchTask::Artist {
        key: artist_key(&source.name, source.mbid.as_deref()),
        source_id,
        artist: source.name.clone(),
        artist_mbid: source.mbid.clone(),
        existing,
    };
    let stored = persist_response(
        store,
        &task,
        &FetchResponse::Artist(response.clone()),
        now,
        storage_scope,
        Some(budget_bytes),
    )?;
    if !stored {
        return Ok(None);
    }
    let new_hash = store
        .artist_observation(source_id)?
        .map(|value| value.payload_hash);
    Ok(Some(old_hash != new_hash))
}

fn validation<T>(
    response: &ResponseEnvelope<T>,
    requested_limit: Option<u32>,
    now: i64,
    expiry: i64,
) -> ObservationValidation {
    ObservationValidation {
        fetched_at: (response.status == ResponseStatus::Modified).then_some(now),
        validated_at: now,
        expires_at: expiry,
        etag: response.headers.etag.clone(),
        last_modified: response.headers.last_modified.clone(),
        requested_limit,
        actual_result_count: (response.status == ResponseStatus::Modified)
            .then_some(response.actual_result_count),
    }
}

fn observation_write<T>(
    response: &ResponseEnvelope<T>,
    hash: String,
    now: i64,
    expiry: i64,
) -> ArtistObservationWrite {
    ArtistObservationWrite {
        fetched_at: now,
        validated_at: now,
        changed_at: now,
        expires_at: expiry,
        etag: response.headers.etag.clone(),
        last_modified: response.headers.last_modified.clone(),
        payload_hash: hash,
        acquisition_source: "api_key".to_owned(),
        requested_limit: response.requested_limit,
        actual_result_count: response.actual_result_count,
        result_status: if response.actual_result_count == 0 {
            ObservationStatus::Empty
        } else {
            ObservationStatus::Positive
        },
    }
}

impl DirectRelation {
    fn matches(&self, candidate: &Candidate) -> bool {
        match (&self.track, &self.artist) {
            (Some(track), None) => {
                track.recording_mbid.as_deref().is_some_and(|mbid| {
                    candidate
                        .recording_mbid
                        .as_deref()
                        .is_some_and(|candidate_mbid| candidate_mbid.eq_ignore_ascii_case(mbid))
                }) || (normalize(candidate.artist.as_deref().unwrap_or_default())
                    == normalize(&track.artist)
                    && normalize(candidate.title.as_deref().unwrap_or_default())
                        == normalize(&track.title))
            }
            (None, Some(artist)) => {
                artist.mbid.as_deref().is_some_and(|mbid| {
                    candidate
                        .artist_mbids
                        .iter()
                        .any(|candidate_mbid| candidate_mbid.eq_ignore_ascii_case(mbid))
                }) || normalize(candidate.artist.as_deref().unwrap_or_default())
                    == normalize(&artist.name)
            }
            _ => false,
        }
    }
}

struct FetchBatch {
    values: Vec<(FetchTask, FetchResponse)>,
    request_count: u64,
    failure_count: u64,
    failures: Vec<(FetchTask, String)>,
}

fn fetch_concurrently(
    options: &DirectOptions,
    api_key: &str,
    deadline: Instant,
    tasks: Vec<FetchTask>,
) -> FetchBatch {
    if tasks.is_empty() {
        return FetchBatch {
            values: Vec::new(),
            request_count: 0,
            failure_count: 0,
            failures: Vec::new(),
        };
    }
    let worker_count = options.max_concurrent_requests.min(tasks.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(tasks)));
    let results = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let request_count = Arc::new(AtomicU64::new(0));
    let failure_count = Arc::new(AtomicU64::new(0));
    thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let request_count = Arc::clone(&request_count);
            let failure_count = Arc::clone(&failure_count);
            let failures = Arc::clone(&failures);
            let endpoint = options.api_endpoint.clone();
            scope.spawn(move || loop {
                let task = queue
                    .lock()
                    .expect("direct queue lock poisoned")
                    .pop_front();
                let Some(task) = task else { break };
                match fetch_with_retry(
                    &task,
                    &endpoint,
                    api_key,
                    deadline,
                    options.artist_result_limit,
                    &request_count,
                ) {
                    Ok(value) => results
                        .lock()
                        .expect("direct result lock poisoned")
                        .push((task, value)),
                    Err(error) => {
                        failure_count.fetch_add(1, Ordering::Relaxed);
                        failures
                            .lock()
                            .expect("direct failure lock poisoned")
                            .push((task, error));
                    }
                }
            });
        }
    });
    let mut results = std::mem::take(&mut *results.lock().expect("direct result lock poisoned"));
    results.sort_unstable_by(|left, right| left.0.key().cmp(right.0.key()));
    let mut failures = std::mem::take(&mut *failures.lock().expect("direct failure lock poisoned"));
    failures.sort_unstable_by(|left, right| left.0.key().cmp(right.0.key()));
    FetchBatch {
        values: results,
        request_count: request_count.load(Ordering::Relaxed),
        failure_count: failure_count.load(Ordering::Relaxed),
        failures,
    }
}

fn remaining_duration(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "direct Last.fm prepare deadline exceeded".to_owned())
}

fn fetch_with_retry(
    task: &FetchTask,
    endpoint: &str,
    api_key: &str,
    deadline: Instant,
    artist_limit: ArtistLimit,
    request_count: &AtomicU64,
) -> Result<FetchResponse, String> {
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        request_count.fetch_add(1, Ordering::Relaxed);
        match task.fetch(endpoint, api_key, deadline, artist_limit) {
            Ok(value) => return Ok(value),
            Err(error) if error.is_retryable() && attempt < MAX_TRANSIENT_RETRIES => {
                let delay = Duration::from_millis(RETRY_BACKOFF_MILLIS * u64::from(attempt + 1));
                let remaining = remaining_duration(deadline)?;
                thread::sleep(delay.min(remaining));
            }
            Err(error) => return Err(error.diagnostic()),
        }
    }
    Err("direct Last.fm request is temporarily unavailable".to_owned())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn track_key(artist: &str, title: &str, mbid: Option<&str>) -> String {
    mbid.filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| format!("{}|{}", normalize(artist), normalize(title)))
}

fn artist_key(artist: &str, mbid: Option<&str>) -> String {
    mbid.filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| normalize(artist))
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

fn integer_option(
    object: &serde_json::Map<String, Value>,
    key: &str,
    minimum: u64,
    maximum: u64,
) -> Result<u64, String> {
    let value = object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value >= minimum && *value <= maximum)
        .ok_or_else(|| format!("direct Last.fm option '{key}' is invalid"))?;
    Ok(value)
}

fn optional_refresh_days(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<u32>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("direct Last.fm option '{key}' is invalid")),
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;

    #[test]
    fn warning_survives_when_info_events_fill_the_diagnostic_budget() {
        let mut events = Vec::new();
        for _ in 0..MAX_LOG_EVENTS {
            push_log_event(&mut events, "info", "routine", None, None, None, None, None);
        }
        push_log_event(
            &mut events,
            "warning",
            "failure",
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(events.len(), MAX_LOG_EVENTS);
        assert!(events.iter().any(|event| event["event"] == "failure"));
    }
}
