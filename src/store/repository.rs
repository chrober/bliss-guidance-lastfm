// SPDX-License-Identifier: GPL-3.0-only

use super::schema;
use super::{normalize_name, ArtistIdentity};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtistCleanupReport {
    pub repaired_names: usize,
    pub removed_orphans: usize,
    pub ambiguous_names: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationValidation {
    pub fetched_at: Option<i64>,
    pub validated_at: i64,
    pub expires_at: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub requested_limit: Option<u32>,
    pub actual_result_count: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtistEdgeWrite {
    pub target_artist_id: i64,
    pub rank: u32,
    pub match_score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtistEdgeRecord {
    pub target_artist_id: i64,
    pub rank: u32,
    pub match_score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtistRecord {
    pub artist_id: i64,
    pub mbid: Option<String>,
    pub canonical_name: String,
    pub normalized_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LegacyArtistTarget {
    pub identity: ArtistIdentity,
    pub rank: u32,
    pub match_score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LegacyArtistObservation {
    pub source: ArtistIdentity,
    pub observation: ArtistObservationWrite,
    pub targets: Vec<LegacyArtistTarget>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LegacyGraphImport {
    pub artists: Vec<LegacyArtistObservation>,
}

#[derive(Debug)]
pub struct GraphStore {
    connection: Connection,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphStatistics {
    pub artists: u64,
    pub artist_observations: u64,
    pub artist_edges: u64,
    pub library_artists: u64,
    pub covered_library_artists: u64,
    pub external_artists: u64,
    pub reciprocal_artist_pairs: u64,
}

#[derive(Serialize)]
struct RetainedPayload<T> {
    etag: Option<String>,
    last_modified: Option<String>,
    results: Vec<T>,
}

#[derive(Serialize)]
struct RetainedArtistResult {
    mbid: Option<String>,
    name: String,
    rank: u32,
    score: Option<f64>,
}

impl GraphStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_at(path, unix_seconds())
    }

    pub fn open_at(path: impl AsRef<Path>, created_at: i64) -> Result<Self, String> {
        Self::open_at_with_timeout(path, created_at, schema::BUSY_TIMEOUT_MILLIS)
    }

    pub fn open_for_maintenance(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_at_with_timeout(path, unix_seconds(), 30_000)
    }

    fn open_at_with_timeout(
        path: impl AsRef<Path>,
        created_at: i64,
        timeout_millis: i64,
    ) -> Result<Self, String> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create Last.fm graph directory: {error}"))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| format!("cannot open Last.fm graph database: {error}"))?;
        schema::initialize(&connection, created_at, timeout_millis)?;
        backfill_retained_payload_bytes(&connection)?;
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

    pub fn statistics(&self) -> Result<GraphStatistics, String> {
        let current_snapshot = "(SELECT snapshot_id FROM library_snapshots ORDER BY created_at DESC, snapshot_id DESC LIMIT 1)";
        Ok(GraphStatistics {
            artists: self.count("artists")?,
            artist_observations: self.count("artist_queries")?,
            artist_edges: self.count("artist_similarity_edges")?,
            library_artists: self.scalar_count(&format!(
                "SELECT COUNT(*) FROM library_artist_membership WHERE snapshot_id = {current_snapshot}"
            ))?,
            covered_library_artists: self.scalar_count(&format!(
                "SELECT COUNT(*) FROM artist_queries WHERE source_artist_id IN (SELECT artist_id FROM library_artist_membership WHERE snapshot_id = {current_snapshot})"
            ))?,
            external_artists: self.scalar_count(&format!(
                "SELECT COUNT(*) FROM artists WHERE artist_id NOT IN (SELECT artist_id FROM library_artist_membership WHERE snapshot_id = {current_snapshot})"
            ))?,
            reciprocal_artist_pairs: self.scalar_count(
                "SELECT COUNT(*) FROM artist_similarity_edges first_edge JOIN artist_queries first_query ON first_query.query_id = first_edge.query_id JOIN artist_queries reverse_query ON reverse_query.source_artist_id = first_edge.target_artist_id JOIN artist_similarity_edges reverse_edge ON reverse_edge.query_id = reverse_query.query_id AND reverse_edge.target_artist_id = first_query.source_artist_id WHERE first_query.source_artist_id < reverse_query.source_artist_id",
            )?,
        })
    }

    pub fn retained_lastfm_bytes(&self) -> Result<u64, String> {
        self.connection
            .query_row(
                "SELECT COALESCE(SUM(retained_payload_bytes), 0) FROM artist_queries",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value.max(0) as u64)
            .map_err(|error| format!("cannot query retained Last.fm data size: {error}"))
    }

    fn scalar_count(&self, sql: &str) -> Result<u64, String> {
        self.connection
            .query_row(sql, [], |row| row.get::<_, i64>(0))
            .map(|value| value.max(0) as u64)
            .map_err(|error| format!("cannot query Last.fm graph statistics: {error}"))
    }

    fn count(&self, table: &str) -> Result<u64, String> {
        self.connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|value| value.max(0) as u64)
            .map_err(|error| format!("cannot count Last.fm graph {table}: {error}"))
    }

    pub fn replace_library_membership(
        &mut self,
        snapshot_id: &str,
        library_identity: &str,
        created_at: i64,
        artist_ids: &[i64],
    ) -> Result<(), String> {
        let transaction = self.connection.transaction().map_err(|error| {
            format!("cannot start Last.fm library-membership transaction: {error}")
        })?;
        transaction
            .execute("DELETE FROM library_artist_membership", [])
            .map_err(|error| format!("cannot clear prior Last.fm library membership: {error}"))?;
        transaction
            .execute("DELETE FROM library_snapshots", [])
            .map_err(|error| format!("cannot clear prior Last.fm library snapshot: {error}"))?;
        transaction
            .execute(
                "INSERT OR REPLACE INTO library_snapshots(snapshot_id, library_identity, created_at) VALUES (?1, ?2, ?3)",
                params![snapshot_id, library_identity, created_at],
            )
            .map_err(|error| format!("cannot store Last.fm library snapshot: {error}"))?;
        for artist_id in artist_ids {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO library_artist_membership(snapshot_id, artist_id) VALUES (?1, ?2)",
                    params![snapshot_id, artist_id],
                )
                .map_err(|error| format!("cannot store Last.fm library artist: {error}"))?;
        }
        transaction
            .commit()
            .map_err(|error| format!("cannot commit Last.fm library membership: {error}"))
    }

    pub fn refresh_library_membership(
        &mut self,
        snapshot_id: &str,
        identities: &[ArtistIdentity],
        now: i64,
    ) -> Result<(), String> {
        self.refresh_library_membership_with_identity(snapshot_id, snapshot_id, identities, now)
    }

    pub fn refresh_library_membership_with_identity(
        &mut self,
        snapshot_id: &str,
        library_identity: &str,
        identities: &[ArtistIdentity],
        now: i64,
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| format!("cannot start library membership refresh: {e}"))?;
        transaction
            .execute("DELETE FROM library_artist_membership", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM library_snapshots", [])
            .map_err(|e| e.to_string())?;
        transaction.execute("INSERT INTO library_snapshots(snapshot_id, library_identity, created_at) VALUES (?1, ?2, ?3)", params![snapshot_id, library_identity, now]).map_err(|e| e.to_string())?;
        for identity in identities {
            let id = resolve_or_create_artist_on(&transaction, identity, now)?;
            transaction.execute("INSERT OR IGNORE INTO library_artist_membership(snapshot_id, artist_id) VALUES (?1, ?2)", params![snapshot_id, id]).map_err(|e| e.to_string())?;
        }
        transaction
            .commit()
            .map_err(|e| format!("cannot commit library membership refresh: {e}"))
    }

    pub fn library_membership_is_current(&self, identity: &str) -> Result<bool, String> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM library_snapshots WHERE library_identity = ?1)",
                [identity],
                |row| row.get(0),
            )
            .map_err(|e| format!("cannot inspect library membership: {e}"))
    }

    pub fn checkpoint_and_compact(
        &mut self,
        artist_result_limit: u32,
        local_artists_only: bool,
    ) -> Result<usize, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| {
                format!("cannot start Last.fm graph compaction transaction: {error}")
            })?;
        if artist_result_limit > 0 {
            transaction
                .execute(
                    "DELETE FROM artist_similarity_edges WHERE rank > ?1",
                    [artist_result_limit],
                )
                .map_err(|error| format!("cannot trim Last.fm artist edges: {error}"))?;
        }
        if local_artists_only {
            let has_snapshot: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_snapshots)",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| format!("cannot inspect Last.fm library snapshot: {error}"))?;
            if !has_snapshot {
                return Err("cannot apply local-only cleanup without a library snapshot".to_owned());
            }
            transaction.execute(
                "DELETE FROM artist_queries WHERE source_artist_id NOT IN (SELECT artist_id FROM library_artist_membership WHERE snapshot_id = (SELECT snapshot_id FROM library_snapshots ORDER BY created_at DESC, snapshot_id DESC LIMIT 1))", [],
            ).map_err(|error| format!("cannot trim non-library Last.fm artist queries: {error}"))?;
            transaction
                .execute(
                    "DELETE FROM artist_similarity_edges WHERE target_artist_id NOT IN (SELECT artist_id FROM library_artist_membership WHERE snapshot_id = (SELECT snapshot_id FROM library_snapshots ORDER BY created_at DESC, snapshot_id DESC LIMIT 1))",
                    [],
                )
                .map_err(|error| format!("cannot trim non-library Last.fm artist edges: {error}"))?;
        }
        let removed = if local_artists_only {
            transaction.execute(
            "DELETE FROM artists WHERE NOT EXISTS (SELECT 1 FROM library_artist_membership m WHERE m.artist_id = artists.artist_id) AND NOT EXISTS (SELECT 1 FROM artist_queries q WHERE q.source_artist_id = artists.artist_id) AND NOT EXISTS (SELECT 1 FROM artist_similarity_edges e WHERE e.target_artist_id = artists.artist_id)", [],
        ).map_err(|error| format!("cannot remove orphaned Last.fm artists: {error}"))?
        } else {
            0
        };
        recalculate_all_artist_payload_bytes(&transaction)?;
        transaction
            .commit()
            .map_err(|error| format!("cannot commit Last.fm graph compaction: {error}"))?;
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
            .map_err(|error| format!("cannot compact Last.fm graph: {error}"))?;
        Ok(removed)
    }

    pub fn cleanup_artist_records(
        &mut self,
        snapshot: &crate::root_snapshot::LibraryArtistSnapshot,
    ) -> Result<ArtistCleanupReport, String> {
        snapshot.validate()?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| format!("cannot start artist cleanup: {e}"))?;
        let mut by_mojibake: HashMap<String, Vec<&crate::root_snapshot::LibraryArtistRoot>> =
            HashMap::new();
        let mut protected_names = HashSet::new();
        let mut protected_mbids = HashSet::new();
        for root in &snapshot.artists {
            protected_names.insert(normalize_name(&root.name));
            for mbid in &root.artist_mbids {
                protected_mbids.insert(mbid.to_ascii_lowercase());
            }
            let wrong: String = root
                .name
                .as_bytes()
                .iter()
                .map(|byte| char::from(*byte))
                .collect();
            if wrong != root.name {
                by_mojibake.entry(wrong).or_default().push(root);
            }
        }
        let artists = {
            let mut statement = transaction
                .prepare(
                    "SELECT artist_id, artist_mbid, canonical_name, normalized_name FROM artists",
                )
                .map_err(|e| format!("cannot inspect graph artists: {e}"))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| format!("cannot query graph artists: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("cannot read graph artist: {e}"))?
        };
        let mut report = ArtistCleanupReport::default();
        for (id, mbid, name, old_normalized) in &artists {
            let Some(roots) = by_mojibake.get(name) else {
                continue;
            };
            if protected_names.contains(old_normalized) {
                report.ambiguous_names += 1;
                continue;
            }
            let matches: Vec<_> = roots
                .iter()
                .filter(|root| match mbid.as_deref() {
                    Some(value) if !value.is_empty() => root
                        .artist_mbids
                        .iter()
                        .any(|id| id.eq_ignore_ascii_case(value)),
                    _ => true,
                })
                .collect();
            if matches.len() != 1 {
                report.ambiguous_names += 1;
                continue;
            }
            let corrected = &matches[0].name;
            let normalized = normalize_name(corrected);
            let collision: i64 = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM artists WHERE artist_id != ?1 AND normalized_name = ?2)",
                params![id, normalized], |row| row.get(0)).map_err(|e| format!("cannot check artist cleanup collision: {e}"))?;
            if collision != 0 && mbid.as_deref().is_none_or(str::is_empty) {
                report.ambiguous_names += 1;
                continue;
            }
            transaction.execute("UPDATE artists SET canonical_name = ?1, normalized_name = ?2 WHERE artist_id = ?3", params![corrected, normalized, id])
                .map_err(|e| format!("cannot repair artist name: {e}"))?;
            transaction.execute("UPDATE artist_queries SET expires_at = 0, validated_at = 0, etag = NULL, last_modified = NULL WHERE source_artist_id = ?1", [id])
                .map_err(|e| format!("cannot schedule repaired artist for revalidation: {e}"))?;
            transaction.execute("DELETE FROM artist_aliases WHERE artist_id = ?1 AND alias_kind = 'name' AND normalized_value = ?2", params![id, old_normalized])
                .map_err(|e| format!("cannot remove malformed artist alias: {e}"))?;
            transaction.execute("INSERT OR IGNORE INTO artist_aliases(artist_id, alias_kind, display_value, normalized_value) VALUES (?1, 'name', ?2, ?3)", params![id, corrected, normalized])
                .map_err(|e| format!("cannot add corrected artist alias: {e}"))?;
            report.repaired_names += 1;
        }
        for (id, mbid, name, _) in &artists {
            if protected_names.contains(&normalize_name(name))
                || by_mojibake.contains_key(name)
                || mbid
                    .as_ref()
                    .is_some_and(|value| protected_mbids.contains(&value.to_ascii_lowercase()))
            {
                continue;
            }
            transaction.execute(
                "DELETE FROM library_artist_membership WHERE artist_id = ?1 AND NOT EXISTS (SELECT 1 FROM artist_queries WHERE source_artist_id = ?1) AND NOT EXISTS (SELECT 1 FROM artist_similarity_edges WHERE target_artist_id = ?1)",
                [id],
            ).map_err(|e| format!("cannot remove obsolete library membership: {e}"))?;
            report.removed_orphans += transaction.execute(
                "DELETE FROM artists WHERE artist_id = ?1 AND NOT EXISTS (SELECT 1 FROM library_artist_membership WHERE artist_id = ?1) AND NOT EXISTS (SELECT 1 FROM artist_queries WHERE source_artist_id = ?1) AND NOT EXISTS (SELECT 1 FROM artist_similarity_edges WHERE target_artist_id = ?1)",
                [id]).map_err(|e| format!("cannot remove orphaned artist: {e}"))?;
        }
        transaction
            .commit()
            .map_err(|e| format!("cannot commit artist cleanup: {e}"))?;
        Ok(report)
    }

    pub fn upsert_artist(
        &self,
        artist_mbid: Option<&str>,
        canonical_name: &str,
        now: i64,
    ) -> Result<i64, String> {
        self.resolve_or_create_artist(
            &ArtistIdentity {
                mbid: artist_mbid.map(str::to_owned),
                name: canonical_name.to_owned(),
            },
            now,
        )
    }

    pub fn resolve_or_create_artist(
        &self,
        identity: &ArtistIdentity,
        now: i64,
    ) -> Result<i64, String> {
        let mbid = normalized_mbid(identity.mbid.as_deref());
        let name = identity.name.trim();
        if name.is_empty() {
            return Err("Last.fm graph artist name is empty".to_owned());
        }
        let normalized = normalize_name(name);
        let existing = if let Some(mbid) = mbid.as_deref() {
            let by_mbid = self
                .connection
                .query_row(
                    "SELECT artist_id FROM artists WHERE artist_mbid = ?1",
                    [mbid],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| format!("cannot resolve Last.fm graph artist MBID: {error}"))?;
            if by_mbid.is_some() {
                by_mbid
            } else {
                let name_only = self
                    .connection
                    .query_row(
                        "SELECT artist_id FROM artists WHERE artist_mbid IS NULL AND normalized_name = ?1",
                        [&normalized],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(|error| format!("cannot resolve Last.fm name-only artist: {error}"))?;
                if let Some(artist_id) = name_only {
                    self.connection
                        .execute(
                            "UPDATE artists SET artist_mbid = ?1 WHERE artist_id = ?2",
                            params![mbid, artist_id],
                        )
                        .map_err(|error| {
                            format!("cannot promote Last.fm graph artist identity: {error}")
                        })?;
                }
                name_only
            }
        } else {
            self.connection
                .query_row(
                    "SELECT artist_id FROM artists WHERE artist_mbid IS NULL AND normalized_name = ?1",
                    [&normalized],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| format!("cannot resolve Last.fm graph artist: {error}"))?
        };
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

    pub fn artist(&self, artist_id: i64) -> Result<Option<ArtistRecord>, String> {
        self.connection
            .query_row(
                "SELECT artist_mbid, canonical_name, normalized_name FROM artists WHERE artist_id = ?1",
                [artist_id],
                |row| {
                    Ok(ArtistRecord {
                        artist_id,
                        mbid: row.get(0)?,
                        canonical_name: row.get(1)?,
                        normalized_name: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|error| format!("cannot read Last.fm graph artist: {error}"))
    }

    pub fn find_artist(&self, identity: &ArtistIdentity) -> Result<Option<i64>, String> {
        Ok(self.find_artists(std::slice::from_ref(identity))?.remove(0))
    }

    pub fn is_library_artist(&self, artist_id: i64) -> Result<bool, String> {
        self.connection
            .query_row(
                "SELECT 1 FROM library_artist_membership WHERE snapshot_id = (SELECT snapshot_id FROM library_snapshots ORDER BY created_at DESC, snapshot_id DESC LIMIT 1) AND artist_id = ?1 LIMIT 1",
                [artist_id],
                |_| Ok(()),
            )
            .optional()
            .map(|value| value.is_some())
            .map_err(|error| format!("cannot inspect Last.fm library membership: {error}"))
    }

    pub fn find_artists(&self, identities: &[ArtistIdentity]) -> Result<Vec<Option<i64>>, String> {
        if identities.is_empty() {
            return Ok(Vec::new());
        }
        let mut mbids = identities
            .iter()
            .filter_map(|identity| normalized_mbid(identity.mbid.as_deref()))
            .collect::<Vec<_>>();
        mbids.sort_unstable();
        mbids.dedup();
        let mut names = identities
            .iter()
            .map(|identity| normalize_name(&identity.name))
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();

        let mbid_placeholders = std::iter::repeat_n("?", mbids.len())
            .collect::<Vec<_>>()
            .join(",");
        let name_placeholders = std::iter::repeat_n("?", names.len())
            .collect::<Vec<_>>()
            .join(",");
        let mbid_clause = if mbids.is_empty() {
            String::new()
        } else {
            format!("artist_mbid IN ({mbid_placeholders}) OR ")
        };
        let sql = format!(
            "SELECT artist_id, artist_mbid, normalized_name FROM artists WHERE {mbid_clause}normalized_name IN ({name_placeholders})"
        );
        let parameters = mbids
            .iter()
            .chain(names.iter())
            .cloned()
            .collect::<Vec<_>>();
        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(|error| format!("cannot prepare batched Last.fm artist lookup: {error}"))?;
        let rows = statement
            .query_map(params_from_iter(parameters), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| format!("cannot query batched Last.fm artists: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot decode batched Last.fm artist: {error}"))?;
        let mut by_mbid = HashMap::new();
        let mut by_name = HashMap::<String, Vec<(i64, Option<String>)>>::new();
        for (artist_id, mbid, normalized_name) in rows {
            if let Some(mbid) = mbid.as_deref() {
                by_mbid.insert(mbid.to_owned(), artist_id);
            }
            by_name
                .entry(normalized_name)
                .or_default()
                .push((artist_id, mbid));
        }
        Ok(identities
            .iter()
            .map(|identity| {
                let mbid = normalized_mbid(identity.mbid.as_deref());
                if let Some(artist_id) = mbid.as_ref().and_then(|value| by_mbid.get(value)) {
                    return Some(*artist_id);
                }
                let candidates = by_name.get(&normalize_name(&identity.name))?;
                if mbid.is_some() {
                    let name_only = candidates
                        .iter()
                        .filter(|(_, candidate_mbid)| candidate_mbid.is_none())
                        .collect::<Vec<_>>();
                    (name_only.len() == 1).then_some(name_only[0].0)
                } else {
                    (candidates.len() == 1).then_some(candidates[0].0)
                }
            })
            .collect())
    }

    pub fn replace_artist_observation(
        &mut self,
        source_artist_id: i64,
        observation: &ArtistObservationWrite,
        edges: &[ArtistEdgeWrite],
    ) -> Result<(), String> {
        self.replace_artist_observation_with_budget(source_artist_id, observation, edges, u64::MAX)?
            .then_some(())
            .ok_or_else(|| "Last.fm retained-data budget is exhausted".to_owned())
    }

    pub fn replace_artist_observation_with_budget(
        &mut self,
        source_artist_id: i64,
        observation: &ArtistObservationWrite,
        edges: &[ArtistEdgeWrite],
        budget_bytes: u64,
    ) -> Result<bool, String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("cannot start Last.fm graph transaction: {error}"))?;
        replace_artist_observation_on(&transaction, source_artist_id, observation, edges)?;
        if retained_lastfm_bytes_on(&transaction)? > budget_bytes {
            return Ok(false);
        }
        transaction
            .commit()
            .map_err(|error| format!("cannot commit Last.fm artist observation: {error}"))?;
        Ok(true)
    }

    pub fn revalidate_artist_observation(
        &mut self,
        source_artist_id: i64,
        validation: &ObservationValidation,
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("cannot start Last.fm artist revalidation: {error}"))?;
        let changed = transaction
            .execute(
                r#"
                UPDATE artist_queries SET
                    fetched_at = COALESCE(?2, fetched_at),
                    validated_at = ?3,
                    expires_at = ?4,
                    etag = COALESCE(?5, etag),
                    last_modified = COALESCE(?6, last_modified),
                    requested_limit = COALESCE(?7, requested_limit),
                    actual_result_count = COALESCE(?8, actual_result_count)
                WHERE source_artist_id = ?1
                "#,
                params![
                    source_artist_id,
                    validation.fetched_at,
                    validation.validated_at,
                    validation.expires_at,
                    validation.etag,
                    validation.last_modified,
                    validation.requested_limit,
                    validation.actual_result_count,
                ],
            )
            .map_err(|error| format!("cannot revalidate Last.fm artist observation: {error}"))?;
        if changed == 0 {
            return Err("cannot revalidate missing Last.fm artist observation".to_owned());
        }
        let query_id = transaction
            .query_row(
                "SELECT query_id FROM artist_queries WHERE source_artist_id = ?1",
                [source_artist_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("cannot resolve revalidated Last.fm artist query: {error}"))?;
        update_artist_payload_bytes(&transaction, query_id)?;
        transaction
            .commit()
            .map_err(|error| format!("cannot commit Last.fm artist revalidation: {error}"))
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

    pub fn migrated_from_json_at(&self) -> Result<Option<i64>, String> {
        self.connection
            .query_row(
                "SELECT migrated_from_json_at FROM metadata WHERE metadata_id = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("cannot read Last.fm graph migration state: {error}"))
    }

    pub fn import_legacy_graph(
        &mut self,
        import: &LegacyGraphImport,
        migrated_at: i64,
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("cannot start Last.fm cache migration: {error}"))?;
        for entry in &import.artists {
            let source = resolve_or_create_artist_on(
                &transaction,
                &entry.source,
                entry.observation.fetched_at,
            )?;
            let mut edges = Vec::with_capacity(entry.targets.len());
            for target in &entry.targets {
                let target_artist_id = resolve_or_create_artist_on(
                    &transaction,
                    &target.identity,
                    entry.observation.fetched_at,
                )?;
                edges.push(ArtistEdgeWrite {
                    target_artist_id,
                    rank: target.rank,
                    match_score: target.match_score,
                });
            }
            replace_artist_observation_on(&transaction, source, &entry.observation, &edges)?;
        }
        transaction
            .execute(
                "UPDATE metadata SET migrated_from_json_at = ?1 WHERE metadata_id = 1",
                [migrated_at],
            )
            .map_err(|error| format!("cannot mark Last.fm cache migration complete: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("cannot commit Last.fm cache migration: {error}"))
    }
}

fn retained_lastfm_bytes_on(connection: &Connection) -> Result<u64, String> {
    connection
        .query_row(
            "SELECT COALESCE(SUM(retained_payload_bytes), 0) FROM artist_queries",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value.max(0) as u64)
        .map_err(|error| format!("cannot query retained Last.fm data size: {error}"))
}

fn resolve_or_create_artist_on(
    connection: &Connection,
    identity: &ArtistIdentity,
    now: i64,
) -> Result<i64, String> {
    let mbid = normalized_mbid(identity.mbid.as_deref());
    let name = identity.name.trim();
    if name.is_empty() {
        return Err("Last.fm graph artist name is empty".to_owned());
    }
    let normalized = normalize_name(name);
    let existing = if let Some(mbid) = mbid.as_deref() {
        let by_mbid = connection
            .query_row(
                "SELECT artist_id FROM artists WHERE artist_mbid = ?1",
                [mbid],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| format!("cannot resolve Last.fm graph artist MBID: {error}"))?;
        if by_mbid.is_some() {
            by_mbid
        } else {
            let name_only = connection
                .query_row(
                    "SELECT artist_id FROM artists WHERE artist_mbid IS NULL AND normalized_name = ?1",
                    [&normalized],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|error| format!("cannot resolve Last.fm name-only artist: {error}"))?;
            if let Some(artist_id) = name_only {
                connection
                    .execute(
                        "UPDATE artists SET artist_mbid = ?1 WHERE artist_id = ?2",
                        params![mbid, artist_id],
                    )
                    .map_err(|error| {
                        format!("cannot promote Last.fm graph artist identity: {error}")
                    })?;
            }
            name_only
        }
    } else {
        connection
            .query_row(
                "SELECT artist_id FROM artists WHERE artist_mbid IS NULL AND normalized_name = ?1",
                [&normalized],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| format!("cannot resolve Last.fm graph artist: {error}"))?
    };
    let artist_id = if let Some(artist_id) = existing {
        connection
            .execute(
                "UPDATE artists SET canonical_name = ?1, normalized_name = ?2, last_seen_at = ?3 WHERE artist_id = ?4",
                params![name, normalized, now, artist_id],
            )
            .map_err(|error| format!("cannot update Last.fm graph artist: {error}"))?;
        artist_id
    } else {
        connection
            .execute(
                "INSERT INTO artists(artist_mbid, canonical_name, normalized_name, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![mbid, name, normalized, now],
            )
            .map_err(|error| format!("cannot insert Last.fm graph artist: {error}"))?;
        connection.last_insert_rowid()
    };
    connection
        .execute(
            "INSERT OR IGNORE INTO artist_aliases(artist_id, alias_kind, display_value, normalized_value) VALUES (?1, 'name', ?2, ?3)",
            params![artist_id, name, normalized],
        )
        .map_err(|error| format!("cannot insert Last.fm graph artist alias: {error}"))?;
    Ok(artist_id)
}

fn replace_artist_observation_on(
    connection: &Connection,
    source_artist_id: i64,
    observation: &ArtistObservationWrite,
    edges: &[ArtistEdgeWrite],
) -> Result<(), String> {
    let query_id: i64 = connection
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
    connection
        .execute(
            "DELETE FROM artist_similarity_edges WHERE query_id = ?1",
            [query_id],
        )
        .map_err(|error| format!("cannot replace Last.fm artist similarity edges: {error}"))?;
    let mut ordered_edges = edges.iter().collect::<Vec<_>>();
    ordered_edges.sort_by_key(|edge| edge.rank);
    let mut seen_targets = HashSet::new();
    for edge in ordered_edges {
        if !seen_targets.insert(edge.target_artist_id) {
            continue;
        }
        connection
            .execute(
                "INSERT INTO artist_similarity_edges(query_id, target_artist_id, rank, match_score) VALUES (?1, ?2, ?3, ?4)",
                params![query_id, edge.target_artist_id, edge.rank, edge.match_score],
            )
            .map_err(|error| format!("cannot insert Last.fm artist similarity edge: {error}"))?;
    }
    update_artist_payload_bytes(connection, query_id)?;
    Ok(())
}

fn backfill_retained_payload_bytes(connection: &Connection) -> Result<(), String> {
    let artist_query_ids = query_ids_with_missing_accounting(connection, "artist_queries")?;
    for query_id in artist_query_ids {
        update_artist_payload_bytes(connection, query_id)?;
    }
    Ok(())
}

fn query_ids_with_missing_accounting(
    connection: &Connection,
    table: &str,
) -> Result<Vec<i64>, String> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT query_id FROM {table} WHERE retained_payload_bytes = 0 ORDER BY query_id"
        ))
        .map_err(|error| format!("cannot prepare Last.fm retained-data backfill: {error}"))?;
    let query_ids = statement
        .query_map([], |row| row.get(0))
        .map_err(|error| format!("cannot query Last.fm retained-data backfill: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read Last.fm retained-data backfill: {error}"))?;
    Ok(query_ids)
}

fn recalculate_all_artist_payload_bytes(connection: &Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare("SELECT query_id FROM artist_queries ORDER BY query_id")
        .map_err(|error| format!("cannot prepare Last.fm artist accounting refresh: {error}"))?;
    let query_ids = statement
        .query_map([], |row| row.get(0))
        .map_err(|error| format!("cannot query Last.fm artist accounting refresh: {error}"))?
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|error| format!("cannot read Last.fm artist accounting refresh: {error}"))?;
    drop(statement);
    for query_id in query_ids {
        update_artist_payload_bytes(connection, query_id)?;
    }
    Ok(())
}

fn update_artist_payload_bytes(connection: &Connection, query_id: i64) -> Result<(), String> {
    let (etag, last_modified) = connection
        .query_row(
            "SELECT etag, last_modified FROM artist_queries WHERE query_id = ?1",
            [query_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| format!("cannot read Last.fm artist validators: {error}"))?;
    let mut statement = connection
        .prepare(
            r#"
            SELECT artists.artist_mbid, artists.canonical_name,
                   artist_similarity_edges.rank, artist_similarity_edges.match_score
            FROM artist_similarity_edges
            JOIN artists ON artists.artist_id = artist_similarity_edges.target_artist_id
            WHERE artist_similarity_edges.query_id = ?1
            ORDER BY artist_similarity_edges.rank, artist_similarity_edges.target_artist_id
            "#,
        )
        .map_err(|error| format!("cannot prepare retained Last.fm artist data: {error}"))?;
    let results = statement
        .query_map([query_id], |row| {
            Ok(RetainedArtistResult {
                mbid: row.get(0)?,
                name: row.get(1)?,
                rank: row.get(2)?,
                score: row.get(3)?,
            })
        })
        .map_err(|error| format!("cannot query retained Last.fm artist data: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read retained Last.fm artist data: {error}"))?;
    let retained_payload_bytes = serialized_size(&RetainedPayload {
        etag,
        last_modified,
        results,
    })?;
    connection
        .execute(
            "UPDATE artist_queries SET retained_payload_bytes = ?2 WHERE query_id = ?1",
            params![query_id, retained_payload_bytes],
        )
        .map_err(|error| format!("cannot store retained Last.fm artist data size: {error}"))?;
    Ok(())
}

fn serialized_size(value: &impl Serialize) -> Result<i64, String> {
    serde_json::to_vec(value)
        .map(|value| value.len() as i64)
        .map_err(|error| format!("cannot account retained Last.fm data: {error}"))
}

fn normalized_mbid(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
