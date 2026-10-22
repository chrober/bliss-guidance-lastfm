// SPDX-License-Identifier: GPL-3.0-only

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

pub const ROOT_SNAPSHOT_KIND: &str = "lastfm-library-artist-roots-v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LibraryArtistRoot {
    pub lms_contributor_id: u64,
    pub name: String,
    #[serde(default)]
    pub artist_mbids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LibraryArtistSnapshot {
    pub kind: String,
    pub snapshot_id: String,
    pub library_identity: String,
    pub generated_at: i64,
    pub artists: Vec<LibraryArtistRoot>,
}

impl LibraryArtistSnapshot {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read Last.fm artist-root snapshot: {error}"))?;
        let snapshot: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Last.fm artist-root snapshot is invalid: {error}"))?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.kind != ROOT_SNAPSHOT_KIND {
            return Err("Last.fm artist-root snapshot kind is unsupported".to_owned());
        }
        if self.snapshot_id.trim().is_empty() || self.library_identity.trim().is_empty() {
            return Err("Last.fm artist-root snapshot identity is missing".to_owned());
        }
        let mut contributor_ids = BTreeSet::new();
        for artist in &self.artists {
            if artist.name.trim().is_empty() {
                return Err("Last.fm artist-root name is empty".to_owned());
            }
            if !contributor_ids.insert(artist.lms_contributor_id) {
                return Err("Last.fm artist-root contributor is duplicated".to_owned());
            }
        }
        Ok(())
    }
}
