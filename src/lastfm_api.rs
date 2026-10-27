use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtistLimit {
    Unlimited,
    TwentyFive,
    Fifty,
    OneHundred,
    Custom(u32),
}

impl ArtistLimit {
    pub fn value(self) -> u32 {
        match self {
            Self::Unlimited => 0,
            Self::TwentyFive => 25,
            Self::Fifty => 50,
            Self::OneHundred => 100,
            Self::Custom(value) => value,
        }
    }

    pub fn from_value(value: u32) -> Self {
        match value {
            0 => Self::Unlimited,
            25 => Self::TwentyFive,
            50 => Self::Fifty,
            100 => Self::OneHundred,
            value => Self::Custom(value),
        }
    }

    pub fn is_satisfied_by(self, stored_limit: u32) -> bool {
        match self {
            Self::Unlimited => stored_limit == 0,
            _ => stored_limit == 0 || stored_limit >= self.value(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponseHeaders {
    pub cache_control: Option<String>,
    pub expires: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseStatus {
    Modified,
    NotModified,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResponseEnvelope<T> {
    pub status: ResponseStatus,
    pub results: Vec<T>,
    pub requested_limit: u32,
    pub actual_result_count: u32,
    pub headers: ResponseHeaders,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastFmError {
    message: &'static str,
    retryable: bool,
    retry_after_seconds: Option<u64>,
}

impl LastFmError {
    pub fn retryable(message: &'static str) -> Self {
        Self {
            message,
            retryable: true,
            retry_after_seconds: None,
        }
    }

    pub fn permanent(message: &'static str) -> Self {
        Self {
            message,
            retryable: false,
            retry_after_seconds: None,
        }
    }

    fn rate_limited(retry_after_seconds: Option<u64>) -> Self {
        Self {
            message: "direct Last.fm request was rate limited",
            retryable: true,
            retry_after_seconds,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    pub fn message(&self) -> &'static str {
        self.message
    }

    pub fn retry_after_seconds(&self) -> Option<u64> {
        self.retry_after_seconds
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SimilarTrack {
    pub artist: String,
    pub title: String,
    pub artist_mbid: Option<String>,
    pub recording_mbid: Option<String>,
    pub score: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SimilarArtist {
    pub name: String,
    pub mbid: Option<String>,
    pub score: Option<f64>,
}

#[derive(Clone)]
pub struct LastFmClient {
    endpoint: String,
    api_key: String,
    agent: ureq::Agent,
}

impl LastFmClient {
    pub fn new(endpoint: &str, api_key: &str, timeout: Duration) -> Result<Self, String> {
        let endpoint = endpoint.trim_end_matches('/');
        if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
            return Err("trusted Last.fm endpoint is invalid".to_owned());
        }
        if api_key.trim().is_empty() {
            return Err("direct Last.fm API key is unavailable".to_owned());
        }
        Ok(Self {
            endpoint: endpoint.to_owned(),
            api_key: api_key.to_owned(),
            agent: ureq::AgentBuilder::new().timeout(timeout).build(),
        })
    }

    pub fn similar_tracks(
        &self,
        artist: &str,
        title: &str,
        recording_mbid: Option<&str>,
        validators: Option<&Validators>,
    ) -> Result<ResponseEnvelope<SimilarTrack>, LastFmError> {
        let mut request = self
            .base_request("track.getSimilar")
            .query("artist", artist)
            .query("track", title)
            .query("autocorrect", "1")
            .query("limit", "25");
        if let Some(mbid) = recording_mbid.filter(|value| !value.is_empty()) {
            request = request.query("mbid", mbid);
        }
        self.parse_response(request, 25, validators, parse_similar_tracks)
    }

    pub fn similar_artists(
        &self,
        artist: &str,
        artist_mbid: Option<&str>,
        limit: ArtistLimit,
        validators: Option<&Validators>,
    ) -> Result<ResponseEnvelope<SimilarArtist>, LastFmError> {
        let requested_limit = limit.value();
        let mut request = self
            .base_request("artist.getSimilar")
            .query("artist", artist)
            .query("autocorrect", "1");
        if requested_limit > 0 {
            request = request.query("limit", &requested_limit.to_string());
        }
        if let Some(mbid) = artist_mbid.filter(|value| !value.is_empty()) {
            request = request.query("mbid", mbid);
        }
        self.parse_response(request, requested_limit, validators, parse_similar_artists)
    }

    fn base_request(&self, method: &str) -> ureq::Request {
        self.agent
            .get(&self.endpoint)
            .query("method", method)
            .query("api_key", &self.api_key)
            .query("format", "json")
    }

    fn parse_response<T>(
        &self,
        mut request: ureq::Request,
        requested_limit: u32,
        validators: Option<&Validators>,
        parser: fn(&Value) -> Result<Vec<T>, String>,
    ) -> Result<ResponseEnvelope<T>, LastFmError> {
        if let Some(value) = validators.and_then(|value| value.etag.as_deref()) {
            request = request.set("If-None-Match", value);
        }
        if let Some(value) = validators.and_then(|value| value.last_modified.as_deref()) {
            request = request.set("If-Modified-Since", value);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(304, response)) => {
                return Ok(ResponseEnvelope {
                    status: ResponseStatus::NotModified,
                    results: Vec::new(),
                    requested_limit,
                    actual_result_count: 0,
                    headers: response_headers(&response),
                });
            }
            Err(ureq::Error::Status(429, response)) => {
                return Err(LastFmError::rate_limited(retry_after_seconds(&response)));
            }
            Err(ureq::Error::Status(status, _)) if status >= 500 => {
                return Err(LastFmError::retryable(
                    "direct Last.fm request is temporarily unavailable",
                ));
            }
            Err(ureq::Error::Status(_, _)) => {
                return Err(LastFmError::permanent(
                    "direct Last.fm request was rejected",
                ));
            }
            Err(ureq::Error::Transport(_)) => {
                return Err(LastFmError::retryable("direct Last.fm request failed"));
            }
        };
        if response.status() == 304 {
            return Ok(ResponseEnvelope {
                status: ResponseStatus::NotModified,
                results: Vec::new(),
                requested_limit,
                actual_result_count: 0,
                headers: response_headers(&response),
            });
        }
        let headers = response_headers(&response);
        let payload: Value = serde_json::from_reader(response.into_reader())
            .map_err(|_| LastFmError::permanent("direct Last.fm response is invalid"))?;
        if let Some(code) = lastfm_error_code(&payload) {
            if code == 29 {
                return Err(LastFmError::rate_limited(None));
            }
            if code != 6 {
                return Err(LastFmError::permanent(
                    "direct Last.fm request was rejected",
                ));
            }
        }
        let results = parser(&payload)
            .map_err(|_| LastFmError::permanent("direct Last.fm response is invalid"))?;
        Ok(ResponseEnvelope {
            status: ResponseStatus::Modified,
            actual_result_count: results.len() as u32,
            results,
            requested_limit,
            headers,
        })
    }
}

fn retry_after_seconds(response: &ureq::Response) -> Option<u64> {
    response
        .header("Retry-After")
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn lastfm_error_code(payload: &Value) -> Option<u64> {
    payload
        .get("error")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

fn response_headers(response: &ureq::Response) -> ResponseHeaders {
    ResponseHeaders {
        cache_control: response.header("Cache-Control").map(str::to_owned),
        expires: response.header("Expires").map(str::to_owned),
        etag: response.header("ETag").map(str::to_owned),
        last_modified: response.header("Last-Modified").map(str::to_owned),
    }
}

pub fn parse_similar_tracks(payload: &Value) -> Result<Vec<SimilarTrack>, String> {
    if is_not_found(payload) {
        return Ok(Vec::new());
    }
    let tracks = payload
        .get("similartracks")
        .and_then(|value| value.get("track"))
        .and_then(Value::as_array)
        .ok_or_else(|| "Last.fm similar-track response is invalid".to_owned())?;
    Ok(tracks
        .iter()
        .filter_map(|track| {
            let artist = track.get("artist")?.as_object()?;
            let artist_name = artist.get("name")?.as_str()?.trim();
            let title = track.get("name")?.as_str()?.trim();
            (!artist_name.is_empty() && !title.is_empty()).then(|| SimilarTrack {
                artist: artist_name.to_owned(),
                title: title.to_owned(),
                artist_mbid: optional_text(artist.get("mbid")),
                recording_mbid: optional_text(track.get("mbid")),
                score: optional_score(track.get("match")),
            })
        })
        .collect())
}

pub fn parse_similar_artists(payload: &Value) -> Result<Vec<SimilarArtist>, String> {
    if is_not_found(payload) {
        return Ok(Vec::new());
    }
    let artists = payload
        .get("similarartists")
        .and_then(|value| value.get("artist"))
        .and_then(Value::as_array)
        .ok_or_else(|| "Last.fm similar-artist response is invalid".to_owned())?;
    Ok(artists
        .iter()
        .filter_map(|artist| {
            let name = artist.get("name")?.as_str()?.trim();
            (!name.is_empty()).then(|| SimilarArtist {
                name: name.to_owned(),
                mbid: optional_text(artist.get("mbid")),
                score: optional_score(artist.get("match")),
            })
        })
        .collect())
}

fn optional_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_score(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .map(|value| value.clamp(0.0, 1.0))
}

fn is_not_found(payload: &Value) -> bool {
    lastfm_error_code(payload) == Some(6)
}
