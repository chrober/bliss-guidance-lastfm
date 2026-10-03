use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastFmError {
    message: &'static str,
    retryable: bool,
}

impl LastFmError {
    pub(crate) fn retryable(message: &'static str) -> Self {
        Self {
            message,
            retryable: true,
        }
    }

    pub(crate) fn permanent(message: &'static str) -> Self {
        Self {
            message,
            retryable: false,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    pub fn message(&self) -> &'static str {
        self.message
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
    ) -> Result<Vec<SimilarTrack>, LastFmError> {
        let mut request = self
            .base_request("track.getSimilar")
            .query("artist", artist)
            .query("track", title)
            .query("autocorrect", "1");
        if let Some(mbid) = recording_mbid.filter(|value| !value.is_empty()) {
            request = request.query("mbid", mbid);
        }
        parse_similar_tracks(&self.call(request)?)
            .map_err(|_| LastFmError::permanent("direct Last.fm response is invalid"))
    }

    pub fn similar_artists(
        &self,
        artist: &str,
        artist_mbid: Option<&str>,
    ) -> Result<Vec<SimilarArtist>, LastFmError> {
        let mut request = self
            .base_request("artist.getSimilar")
            .query("artist", artist)
            .query("autocorrect", "1")
            .query("limit", "25");
        if let Some(mbid) = artist_mbid.filter(|value| !value.is_empty()) {
            request = request.query("mbid", mbid);
        }
        parse_similar_artists(&self.call(request)?)
            .map_err(|_| LastFmError::permanent("direct Last.fm response is invalid"))
    }

    fn base_request(&self, method: &str) -> ureq::Request {
        self.agent
            .get(&self.endpoint)
            .query("method", method)
            .query("api_key", &self.api_key)
            .query("format", "json")
    }

    fn call(&self, request: ureq::Request) -> Result<Value, LastFmError> {
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(status, _)) if status == 429 || status >= 500 => {
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
        serde_json::from_reader(response.into_reader())
            .map_err(|_| LastFmError::permanent("direct Last.fm response is invalid"))
    }
}

pub fn parse_similar_tracks(payload: &Value) -> Result<Vec<SimilarTrack>, String> {
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
