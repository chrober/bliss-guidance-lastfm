// SPDX-License-Identifier: GPL-3.0-only
//! Private pending artist observations. Callers must integrate this accounting
//! into every retained-data writer before enabling the queue in production.

use crate::lastfm_api::{ResponseEnvelope, SimilarArtist};
use crate::legacy_cache::CacheUsage;
use crate::storage_budget::BudgetLock;
use crate::store::GraphStore;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct RetainedArtist<'a> {
    mbid: &'a Option<String>,
    name: &'a str,
    rank: usize,
    score: Option<f64>,
}
#[derive(Serialize)]
struct RetainedPayload<'a> {
    etag: &'a Option<String>,
    last_modified: &'a Option<String>,
    results: Vec<RetainedArtist<'a>>,
}

fn sanitized(value: &Value) -> Result<(Value, u64), String> {
    let pair = value
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or("invalid pending artist pair")?;
    let task = pair[0]
        .get("Artist")
        .and_then(Value::as_object)
        .ok_or("invalid pending artist task")?;
    let key = task
        .get("key")
        .and_then(Value::as_str)
        .ok_or("missing pending artist key")?;
    let artist = task
        .get("artist")
        .and_then(Value::as_str)
        .ok_or("missing pending artist name")?;
    let mbid = task
        .get("artist_mbid")
        .filter(|v| v.is_null() || v.is_string())
        .ok_or("invalid pending artist MBID")?;
    let response: ResponseEnvelope<SimilarArtist> = serde_json::from_value(
        pair[1]
            .get("Artist")
            .ok_or("invalid pending artist response")?
            .clone(),
    )
    .map_err(|e| format!("invalid pending artist response: {e}"))?;
    let retained = RetainedPayload {
        etag: &response.headers.etag,
        last_modified: &response.headers.last_modified,
        results: response
            .results
            .iter()
            .enumerate()
            .map(|(i, r)| RetainedArtist {
                mbid: &r.mbid,
                name: &r.name,
                rank: i + 1,
                score: r.score,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&retained)
        .map_err(|e| e.to_string())?
        .len() as u64;
    Ok((
        json!([{"Artist":{"key":key,"artist":artist,"artist_mbid":mbid,"source_id":0,"existing":null}}, {"Artist":response}]),
        bytes,
    ))
}

pub fn queue_directory(graph: &Path) -> Result<PathBuf, String> {
    let mut name = graph
        .file_name()
        .ok_or("missing Last.fm graph filename")?
        .to_os_string();
    name.push(".pending-artist-writes");
    Ok(graph.with_file_name(name))
}

fn temporary_hash(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let (hash, pid) = name.split_once(".tmp-")?;
    if hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        && !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
    {
        Some(hash)
    } else {
        None
    }
}

/// Call with the graph's budget lock held. Returned paths are private queue
/// entries; callers must keep the lock through persistence and removal.
pub fn entries_locked(graph: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = queue_directory(graph)?;
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("invalid pending artist directory".into());
    }
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if let Some(hash) = temporary_hash(&path) {
            if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
                return Err("invalid pending artist file".into());
            }
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid interrupted pending artist data: {e}"))?;
            let (safe, _) = sanitized(&value)?;
            if format!("{:x}", Sha256::digest(&bytes)) != hash
                || serde_json::to_vec(&safe).map_err(|e| e.to_string())? != bytes
            {
                return Err("invalid interrupted pending artist data".into());
            }
            let final_path = path.with_extension("json");
            if final_path.exists() {
                if fs::read(&final_path).map_err(|e| e.to_string())? != bytes {
                    return Err("invalid existing pending artist data".into());
                }
                fs::remove_file(&path).map_err(|e| e.to_string())?;
            } else {
                fs::rename(&path, &final_path).map_err(|e| e.to_string())?;
            }
            paths.push(final_path);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("invalid pending artist file".into());
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

pub fn read_entry_locked(path: &Path) -> Result<(Value, u64), String> {
    if fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("invalid pending artist file".into());
    }
    let value: Value = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    sanitized(&value)
}

/// Remove only provider-owned queue entries after an explicitly confirmed delete.
pub fn clear_locked(graph: &Path) -> Result<(), String> {
    let dir = queue_directory(graph)?;
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("invalid pending artist directory".into());
    }
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let (hash, suffix) = match name.split_once('.') {
            Some(v) => v,
            None => continue,
        };
        let owned = hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && (suffix == "json"
                || suffix
                    .strip_prefix("tmp-")
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())));
        if !owned {
            continue;
        }
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("invalid pending artist file".into());
        }
        fs::remove_file(entry.path())
            .map_err(|e| format!("cannot delete pending artist data: {e}"))?;
    }
    match fs::remove_dir(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(e) => Err(format!("cannot remove pending artist directory: {e}")),
    }
}

pub fn queued_bytes(graph: &Path) -> Result<u64, String> {
    let dir = queue_directory(graph)?;
    if fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err("invalid pending artist directory".into());
    }
    if !dir.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json")
            && temporary_hash(&path).is_none()
        {
            continue;
        }
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("invalid pending artist file".into());
        }
        let value: Value = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        total = total.saturating_add(sanitized(&value)?.1);
    }
    Ok(total)
}

pub fn enqueue_artist(
    graph: &Path,
    cache: Option<&str>,
    value: &Value,
    budget: u64,
    now: i64,
) -> Result<bool, String> {
    let (safe, size) = sanitized(value)?;
    let bytes = serde_json::to_vec(&safe).map_err(|e| e.to_string())?;
    let _lock = BudgetLock::acquire(graph)?;
    let store = GraphStore::open_at(graph, now)?;
    let mut cache_usage = CacheUsage::new(cache);
    let dir = queue_directory(graph)?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if fs::symlink_metadata(&dir)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("invalid pending artist directory".into());
    }
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let path = dir.join(format!("{hash}.json"));
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if !metadata.file_type().is_file() || fs::read(&path).map_err(|e| e.to_string())? != bytes {
            return Err("existing pending artist entry is invalid".into());
        }
        return Ok(true);
    }
    let current = store
        .retained_lastfm_bytes()?
        .saturating_add(cache_usage.bytes(now)?)
        .saturating_add(queued_bytes(graph)?);
    if current.saturating_add(size) > budget {
        return Ok(false);
    }
    let temp = dir.join(format!("{hash}.tmp-{}", std::process::id()));
    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut file = open.open(&temp).map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    drop(file);
    fs::rename(temp, path).map_err(|e| e.to_string())?;
    Ok(true)
}
