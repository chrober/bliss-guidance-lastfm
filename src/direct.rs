use crate::cache::PersistentCache;
use crate::lastfm_api::{LastFmClient, SimilarArtist, SimilarTrack};
use bliss_playlist_guidance_spi::{Anchor, Candidate, Diagnostics, ScoreContext};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const API_KEY_ENVIRONMENT: &str = "BLISS_GUIDANCE_LASTFM_API_KEY";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectOptions {
    pub api_endpoint: String,
    pub cache_path: String,
    pub cache_ttl_seconds: u64,
    pub request_deadline_ms: u64,
    pub max_concurrent_requests: usize,
}

impl DirectOptions {
    pub fn from_prepare_options(options: &Value) -> Result<Self, String> {
        let object = options
            .as_object()
            .ok_or_else(|| "direct Last.fm options must be an object".to_owned())?;
        if object.get("acquisition_mode").and_then(Value::as_str) != Some("direct") {
            return Err("direct Last.fm acquisition mode is required".to_owned());
        }
        let cache_path = object
            .get("cache_path")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "trusted Last.fm cache path is required".to_owned())?;
        let api_endpoint = object
            .get("api_endpoint")
            .and_then(Value::as_str)
            .unwrap_or("https://ws.audioscrobbler.com/2.0/")
            .trim_end_matches('/')
            .to_owned();
        if !(api_endpoint.starts_with("https://") || api_endpoint.starts_with("http://")) {
            return Err("trusted Last.fm endpoint is invalid".to_owned());
        }
        let cache_ttl_seconds = integer_option(object, "cache_ttl_seconds", 1, 2_592_000)?;
        let request_deadline_ms = integer_option(object, "request_deadline_ms", 100, 60_000)?;
        let max_concurrent_requests = integer_option(object, "max_concurrent_requests", 1, 8)?;
        Ok(Self {
            api_endpoint,
            cache_path: cache_path.to_owned(),
            cache_ttl_seconds,
            request_deadline_ms,
            max_concurrent_requests: max_concurrent_requests as usize,
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
}

#[derive(Clone, Debug)]
struct DirectRelation {
    channel: &'static str,
    score: f64,
    track: Option<SimilarTrack>,
    artist: Option<SimilarArtist>,
}

impl DirectPrepared {
    pub fn prepare(
        options: &DirectOptions,
        anchors: &[Anchor],
    ) -> Result<(Self, Diagnostics), String> {
        let api_key = options.api_key_from_environment()?;
        let client = LastFmClient::new(
            &options.api_endpoint,
            &api_key,
            Duration::from_millis(options.request_deadline_ms),
        )?;
        let now = unix_seconds();
        let deadline = Instant::now() + Duration::from_millis(options.request_deadline_ms);
        let mut cache = PersistentCache::load(&options.cache_path, options.cache_ttl_seconds)?;
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

        let mut request_count = 0_u64;
        let mut cache_hits = 0_u64;
        let mut tracks = HashMap::<String, Vec<SimilarTrack>>::new();
        for (key, (artist, title, mbid)) in &track_queries {
            let cache_key = format!("track:{key}");
            let value = if let Some(value) = cache.get(&cache_key, now) {
                cache_hits += 1;
                value
            } else {
                ensure_before_deadline(deadline)?;
                let fetched = client.similar_tracks(artist, title, mbid.as_deref())?;
                request_count += 1;
                let value = serde_json::to_value(&fetched)
                    .map_err(|_| "cannot encode direct Last.fm cache value".to_owned())?;
                cache.put(cache_key, value.clone(), now);
                value
            };
            let decoded = serde_json::from_value(value)
                .map_err(|_| "cached Last.fm track relation is invalid".to_owned())?;
            tracks.insert(key.clone(), decoded);
        }
        let mut artists = HashMap::<String, Vec<SimilarArtist>>::new();
        for (key, (artist, mbid)) in &artist_queries {
            let cache_key = format!("artist:{key}");
            let value = if let Some(value) = cache.get(&cache_key, now) {
                cache_hits += 1;
                value
            } else {
                ensure_before_deadline(deadline)?;
                let fetched = client.similar_artists(artist, mbid.as_deref())?;
                request_count += 1;
                let value = serde_json::to_value(&fetched)
                    .map_err(|_| "cannot encode direct Last.fm cache value".to_owned())?;
                cache.put(cache_key, value.clone(), now);
                value
            };
            let decoded = serde_json::from_value(value)
                .map_err(|_| "cached Last.fm artist relation is invalid".to_owned())?;
            artists.insert(key.clone(), decoded);
        }
        cache.save()?;

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
        };
        let relation_count = prepared
            .relations_by_source
            .values()
            .map(Vec::len)
            .sum::<usize>();
        Ok((
            prepared,
            Diagnostics {
                state: Some("fresh".to_owned()),
                request_count,
                failure_count: 0,
                details: Some(serde_json::json!({
                    "acquisition_mode": "direct", "cache_hits": cache_hits,
                    "source_anchors": anchors.len(), "relations": relation_count,
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

fn ensure_before_deadline(deadline: Instant) -> Result<(), String> {
    (Instant::now() < deadline)
        .then_some(())
        .ok_or_else(|| "direct Last.fm prepare deadline exceeded".to_owned())
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
