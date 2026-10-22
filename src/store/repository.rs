// SPDX-License-Identifier: GPL-3.0-only

use super::schema;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationStatus {
    Positive,
    Empty,
}

impl ObservationStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Positive => "positive",
            Self::Empty => "empty",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "positive" => Ok(Self::Positive),
            "empty" => Ok(Self::Empty),
            other => Err(format!("invalid Last.fm observation status {other}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtistObservationWrite {
    pub fetched_at: i64,
    pub validated_at: i64,
    pub changed_at: i64,
    pub expires_at: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub payload_hash: String,
    pub acquisition_source: String,
    pub requested_limit: u32,
    pub actual_result_count: u32,
    pub result_status: ObservationStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtistEdgeWrite {
    pub target_artist_id: i64,
    pub rank: u32,
    pub match_score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtistEdgeRecord {
    pub target_artist_id: i64,
    pub rank: u32,
    pub match_score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtistObservationRecord {
    pub source_artist_id: i64,
    pub fetched_at: i64,
    pub validated_at: i64,
    pub changed_at: i64,
    pub expires_at: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub payload_hash: String,
    pub acquisition_source: String,
    pub requested_limit: u32,
    pub actual_result_count: u32,
    pub result_status: ObservationStatus,
    pub edges: Vec<ArtistEdgeRecord>,
}

#[derive(Debug)]
pub struct GraphStore {
    connection: Connection,
}

impl GraphStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_at(path, unix_seconds())
    }

    pub fn open_at(path: impl AsRef<Path>, created_at: i64) -> Result<Self, String> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create Last.fm graph directory: {error}"))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| format!("cannot open Last.fm graph database: {error}"))?;
        schema::initialize(&connection, created_at)?;
        Ok(Self { connection })
    }

    pub fn schema_version(&self) -> Result<i64, String> {
        self.connection
            .query_row(
                "SELECT schema_version FROM metadata WHERE metadata_id = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("cannot read Last.fm graph schema version: {error}"))
    }

    pub fn journal_mode(&self) -> Result<String, String> {
        self.connection
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .map(|value| value.to_ascii_lowercase())
            .map_err(|error| format!("cannot read Last.fm graph journal mode: {error}"))
    }

    pub fn busy_timeout_millis(&self) -> Result<i64, String> {
        self.connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .map_err(|error| format!("cannot read Last.fm graph busy timeout: {error}"))
    }

    pub fn has_table(&self, table: &str) -> Result<bool, String> {
        self.connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |_| Ok(()),
            )
            .optional()
            .map(|value| value.is_some())
            .map_err(|error| format!("cannot inspect Last.fm graph schema: {error}"))
    }

    pub fn upsert_artist(
        &self,
        artist_mbid: Option<&str>,
        canonical_name: &str,
        now: i64,
    ) -> Result<i64, String> {
        let mbid = artist_mbid
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let name = canonical_name.trim();
        if name.is_empty() {
            return Err("Last.fm graph artist name is empty".to_owned());
        }
        let normalized = normalize(name);
        let existing = if let Some(mbid) = mbid.as_deref() {
            self.connection
                .query_row(
                    "SELECT artist_id FROM artists WHERE artist_mbid = ?1",
                    [mbid],
                    |row| row.get(0),
                )
                .optional()
        } else {
            self.connection
                .query_row(
                    "SELECT artist_id FROM artists WHERE artist_mbid IS NULL AND normalized_name = ?1",
                    [&normalized],
                    |row| row.get(0),
                )
                .optional()
        }
        .map_err(|error| format!("cannot resolve Last.fm graph artist: {error}"))?;
        let artist_id = if let Some(artist_id) = existing {
            self.connection
                .execute(
                    "UPDATE artists SET canonical_name = ?1, normalized_name = ?2, last_seen_at = ?3 WHERE artist_id = ?4",
                    params![name, normalized, now, artist_id],
                )
                .map_err(|error| format!("cannot update Last.fm graph artist: {error}"))?;
            artist_id
        } else {
            self.connection
                .execute(
                    "INSERT INTO artists(artist_mbid, canonical_name, normalized_name, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                    params![mbid, name, normalized, now],
                )
                .map_err(|error| format!("cannot insert Last.fm graph artist: {error}"))?;
            self.connection.last_insert_rowid()
        };
        self.connection
            .execute(
                "INSERT OR IGNORE INTO artist_aliases(artist_id, alias_kind, display_value, normalized_value) VALUES (?1, 'name', ?2, ?3)",
                params![artist_id, name, normalized],
            )
            .map_err(|error| format!("cannot insert Last.fm graph artist alias: {error}"))?;
        Ok(artist_id)
    }

    pub fn replace_artist_observation(
        &mut self,
        source_artist_id: i64,
        observation: &ArtistObservationWrite,
        edges: &[ArtistEdgeWrite],
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("cannot start Last.fm graph transaction: {error}"))?;
        let query_id: i64 = transaction
            .query_row(
                r#"
                INSERT INTO artist_queries(
                    source_artist_id, fetched_at, validated_at, changed_at,
                    expires_at, etag, last_modified, payload_hash,
                    acquisition_source, requested_limit, actual_result_count,
                    result_status
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(source_artist_id) DO UPDATE SET
                    fetched_at = excluded.fetched_at,
                    validated_at = excluded.validated_at,
                    changed_at = excluded.changed_at,
                    expires_at = excluded.expires_at,
                    etag = excluded.etag,
                    last_modified = excluded.last_modified,
                    payload_hash = excluded.payload_hash,
                    acquisition_source = excluded.acquisition_source,
                    requested_limit = excluded.requested_limit,
                    actual_result_count = excluded.actual_result_count,
                    result_status = excluded.result_status
                RETURNING query_id
                "#,
                params![
                    source_artist_id,
                    observation.fetched_at,
                    observation.validated_at,
                    observation.changed_at,
                    observation.expires_at,
                    observation.etag,
                    observation.last_modified,
                    observation.payload_hash,
                    observation.acquisition_source,
                    observation.requested_limit,
                    observation.actual_result_count,
                    observation.result_status.as_str(),
                ],
                |row| row.get(0),
            )
            .map_err(|error| format!("cannot replace Last.fm artist observation: {error}"))?;
        transaction
            .execute(
                "DELETE FROM artist_similarity_edges WHERE query_id = ?1",
                [query_id],
            )
            .map_err(|error| format!("cannot replace Last.fm artist similarity edges: {error}"))?;
        for edge in edges {
            transaction
                .execute(
                    "INSERT INTO artist_similarity_edges(query_id, target_artist_id, rank, match_score) VALUES (?1, ?2, ?3, ?4)",
                    params![query_id, edge.target_artist_id, edge.rank, edge.match_score],
                )
                .map_err(|error| format!("cannot insert Last.fm artist similarity edge: {error}"))?;
        }
        transaction
            .commit()
            .map_err(|error| format!("cannot commit Last.fm artist observation: {error}"))
    }

    pub fn artist_observation(
        &self,
        source_artist_id: i64,
    ) -> Result<Option<ArtistObservationRecord>, String> {
        let row = self
            .connection
            .query_row(
                r#"
                SELECT query_id, fetched_at, validated_at, changed_at, expires_at,
                       etag, last_modified, payload_hash, acquisition_source,
                       requested_limit, actual_result_count, result_status
                FROM artist_queries WHERE source_artist_id = ?1
                "#,
                [source_artist_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, u32>(9)?,
                        row.get::<_, u32>(10)?,
                        row.get::<_, String>(11)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("cannot read Last.fm artist observation: {error}"))?;
        let Some((
            query_id,
            fetched_at,
            validated_at,
            changed_at,
            expires_at,
            etag,
            last_modified,
            payload_hash,
            acquisition_source,
            requested_limit,
            actual_result_count,
            result_status,
        )) = row
        else {
            return Ok(None);
        };
        let mut statement = self
            .connection
            .prepare(
                "SELECT target_artist_id, rank, match_score FROM artist_similarity_edges WHERE query_id = ?1 ORDER BY rank, target_artist_id",
            )
            .map_err(|error| format!("cannot prepare Last.fm artist edge lookup: {error}"))?;
        let edges = statement
            .query_map([query_id], |row| {
                Ok(ArtistEdgeRecord {
                    target_artist_id: row.get(0)?,
                    rank: row.get(1)?,
                    match_score: row.get(2)?,
                })
            })
            .map_err(|error| format!("cannot read Last.fm artist similarity edges: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot decode Last.fm artist similarity edge: {error}"))?;
        Ok(Some(ArtistObservationRecord {
            source_artist_id,
            fetched_at,
            validated_at,
            changed_at,
            expires_at,
            etag,
            last_modified,
            payload_hash,
            acquisition_source,
            requested_limit,
            actual_result_count,
            result_status: ObservationStatus::parse(&result_status)?,
            edges,
        }))
    }
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
