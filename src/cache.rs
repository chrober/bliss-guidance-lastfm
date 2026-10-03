use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const CACHE_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheEntry {
    fetched_at_unix_seconds: u64,
    value: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheFile {
    schema_version: u8,
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Clone, Debug)]
pub struct PersistentCache {
    path: PathBuf,
    ttl_seconds: u64,
    entries: BTreeMap<String, CacheEntry>,
}

impl PersistentCache {
    pub fn load(path: impl AsRef<Path>, ttl_seconds: u64) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let entries = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<CacheFile>(&bytes) {
                Ok(file) if file.schema_version == CACHE_SCHEMA_VERSION => file.entries,
                Ok(_) => BTreeMap::new(),
                Err(_) => BTreeMap::new(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(format!("cannot read trusted Last.fm cache: {error}")),
        };
        Ok(Self {
            path,
            ttl_seconds,
            entries,
        })
    }

    pub fn get(&self, key: &str, now_unix_seconds: u64) -> Option<Value> {
        let entry = self.entries.get(key)?;
        (now_unix_seconds.saturating_sub(entry.fetched_at_unix_seconds) < self.ttl_seconds)
            .then(|| entry.value.clone())
    }

    pub fn put(&mut self, key: impl Into<String>, value: Value, now_unix_seconds: u64) {
        self.entries.insert(
            key.into(),
            CacheEntry {
                fetched_at_unix_seconds: now_unix_seconds,
                value,
            },
        );
    }

    pub fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec(&CacheFile {
            schema_version: CACHE_SCHEMA_VERSION,
            entries: self.entries.clone(),
        })
        .map_err(|error| format!("cannot encode Last.fm cache: {error}"))?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("cannot create trusted Last.fm cache directory: {error}")
            })?;
        }
        let temporary = self.path.with_extension("tmp");
        fs::write(&temporary, bytes)
            .map_err(|error| format!("cannot write trusted Last.fm cache: {error}"))?;
        #[cfg(windows)]
        if self.path.exists() {
            fs::remove_file(&self.path)
                .map_err(|error| format!("cannot replace trusted Last.fm cache: {error}"))?;
        }
        fs::rename(&temporary, &self.path)
            .map_err(|error| format!("cannot finalize trusted Last.fm cache: {error}"))
    }
}
