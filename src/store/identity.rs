// SPDX-License-Identifier: GPL-3.0-only

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtistIdentity {
    pub mbid: Option<String>,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackIdentity {
    pub recording_mbid: Option<String>,
    pub artist: ArtistIdentity,
    pub title: String,
}

pub fn normalize_name(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
