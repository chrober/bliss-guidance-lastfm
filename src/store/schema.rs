// SPDX-License-Identifier: GPL-3.0-only

use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 2;
pub const BUSY_TIMEOUT_MILLIS: i64 = 5_000;

pub fn initialize(connection: &Connection, created_at: i64) -> Result<(), String> {
    connection
        .execute_batch(&format!(
            r#"
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = {BUSY_TIMEOUT_MILLIS};
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS metadata (
                metadata_id INTEGER PRIMARY KEY CHECK (metadata_id = 1),
                schema_version INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                migrated_from_json_at INTEGER
            );

            INSERT OR IGNORE INTO metadata(metadata_id, schema_version, created_at)
            VALUES (1, {SCHEMA_VERSION}, {created_at});

            CREATE TABLE IF NOT EXISTS artists (
                artist_id INTEGER PRIMARY KEY,
                artist_mbid TEXT,
                canonical_name TEXT NOT NULL,
                normalized_name TEXT NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS artists_mbid_unique
                ON artists(artist_mbid) WHERE artist_mbid IS NOT NULL AND artist_mbid != '';
            CREATE UNIQUE INDEX IF NOT EXISTS artists_name_only_unique
                ON artists(normalized_name) WHERE artist_mbid IS NULL;
            CREATE INDEX IF NOT EXISTS artists_normalized_name
                ON artists(normalized_name);

            CREATE TABLE IF NOT EXISTS artist_aliases (
                artist_id INTEGER NOT NULL REFERENCES artists(artist_id) ON DELETE CASCADE,
                alias_kind TEXT NOT NULL,
                display_value TEXT NOT NULL,
                normalized_value TEXT NOT NULL,
                PRIMARY KEY (artist_id, alias_kind, normalized_value)
            );
            CREATE INDEX IF NOT EXISTS artist_aliases_normalized
                ON artist_aliases(normalized_value);

            CREATE TABLE IF NOT EXISTS artist_queries (
                query_id INTEGER PRIMARY KEY,
                source_artist_id INTEGER NOT NULL UNIQUE
                    REFERENCES artists(artist_id) ON DELETE CASCADE,
                fetched_at INTEGER NOT NULL,
                validated_at INTEGER NOT NULL,
                changed_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                etag TEXT,
                last_modified TEXT,
                payload_hash TEXT NOT NULL,
                acquisition_source TEXT NOT NULL,
                requested_limit INTEGER NOT NULL,
                actual_result_count INTEGER NOT NULL,
                result_status TEXT NOT NULL
                    CHECK (result_status IN ('positive', 'empty')),
                retained_payload_bytes INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS artist_queries_expiry
                ON artist_queries(expires_at);

            CREATE TABLE IF NOT EXISTS artist_similarity_edges (
                query_id INTEGER NOT NULL
                    REFERENCES artist_queries(query_id) ON DELETE CASCADE,
                target_artist_id INTEGER NOT NULL
                    REFERENCES artists(artist_id) ON DELETE RESTRICT,
                rank INTEGER NOT NULL CHECK (rank > 0),
                match_score REAL,
                PRIMARY KEY (query_id, target_artist_id)
            );
            CREATE INDEX IF NOT EXISTS artist_similarity_edges_target
                ON artist_similarity_edges(target_artist_id);

            CREATE TABLE IF NOT EXISTS library_snapshots (
                snapshot_id TEXT PRIMARY KEY,
                library_identity TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS library_artist_membership (
                snapshot_id TEXT NOT NULL
                    REFERENCES library_snapshots(snapshot_id) ON DELETE CASCADE,
                artist_id INTEGER NOT NULL
                    REFERENCES artists(artist_id) ON DELETE CASCADE,
                PRIMARY KEY (snapshot_id, artist_id)
            );
            CREATE INDEX IF NOT EXISTS library_artist_membership_artist
                ON library_artist_membership(artist_id);

            "#
        ))
        .map_err(|error| format!("cannot initialize Last.fm graph schema: {error}"))?;

    let actual: i64 = connection
        .query_row(
            "SELECT schema_version FROM metadata WHERE metadata_id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("cannot read Last.fm graph schema version: {error}"))?;
    if actual > SCHEMA_VERSION {
        return Err(format!(
            "unsupported Last.fm graph schema version {actual}; expected {SCHEMA_VERSION}"
        ));
    }
    if actual == 1 {
        add_column_if_missing(
            connection,
            "artist_queries",
            "retained_payload_bytes",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        connection
            .execute_batch(
                r#"
                DROP TABLE IF EXISTS track_similarity_edges;
                DROP TABLE IF EXISTS track_queries;
                DROP TABLE IF EXISTS tracks;
                "#,
            )
            .map_err(|error| {
                format!("cannot remove persisted Last.fm track-similarity data: {error}")
            })?;
        connection
            .execute(
                "UPDATE metadata SET schema_version = ?1 WHERE metadata_id = 1",
                [SCHEMA_VERSION],
            )
            .map_err(|error| format!("cannot record Last.fm graph schema migration: {error}"))?;
    }
    Ok(())
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| format!("cannot inspect Last.fm graph table {table}: {error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("cannot inspect Last.fm graph table {table}: {error}"))?;
    for existing in columns {
        if existing
            .map_err(|error| format!("cannot inspect Last.fm graph table {table}: {error}"))?
            == column
        {
            return Ok(());
        }
    }
    connection
        .execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration};"
        ))
        .map_err(|error| format!("cannot migrate Last.fm graph table {table}: {error}"))
}
