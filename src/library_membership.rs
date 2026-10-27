use crate::store::{ArtistIdentity, GraphStore};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn refresh(library: &Path, graph: &Path) -> Result<(), String> {
    let db = Connection::open_with_flags(library, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot open Lyrion library read-only: {e}"))?;
    let mut statement = db.prepare("SELECT DISTINCT contributors.id, contributors.name, contributors.musicbrainz_id FROM contributors JOIN contributor_track ON contributor_track.contributor = contributors.id JOIN tracks ON tracks.id = contributor_track.track WHERE COALESCE(tracks.remote, 0) = 0 AND contributors.name IS NOT NULL AND contributors.name != '' ORDER BY contributors.id").map_err(|e| format!("cannot read Lyrion library artists: {e}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut identities = Vec::new();
    let mut hash = Sha256::new();
    for row in rows {
        let (id, name, mbid) = row.map_err(|e| e.to_string())?;
        hash.update(id.to_le_bytes());
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        if let Some(value) = &mbid {
            hash.update(value.as_bytes());
        }
        identities.push(ArtistIdentity { name, mbid });
    }
    let snapshot_id = format!("{:x}", hash.finalize());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as i64;
    let mut store = GraphStore::open(graph)?;
    if store.library_membership_is_current(&snapshot_id)? {
        return Ok(());
    }
    store.refresh_library_membership(&snapshot_id, &identities, now)
}

pub fn refresh_coalesced(library: &Path, graph: &Path) -> Result<(), String> {
    let lock_path = graph.with_extension("membership.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|e| format!("cannot open membership refresh lock: {e}"))?;
    if let Err(error) = lock.try_lock_exclusive() {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(format!("cannot lock membership refresh: {error}"));
    }
    refresh(library, graph)
}
