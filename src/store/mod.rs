// SPDX-License-Identifier: GPL-3.0-only

mod identity;
mod repository;
mod schema;

pub use identity::{normalize_name, ArtistIdentity, TrackIdentity};
pub use repository::{
    ArtistEdgeRecord, ArtistEdgeWrite, ArtistObservationRecord, ArtistObservationWrite,
    ArtistRecord, GraphStore, ObservationStatus, TrackRecord,
};
