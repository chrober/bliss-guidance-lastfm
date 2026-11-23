// SPDX-License-Identifier: GPL-3.0-only

use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Serializes retained-data accounting and the corresponding write.
/// Never hold this lock across HTTP requests or while waiting for a scan.
pub struct BudgetLock(File);

impl BudgetLock {
    pub fn path_for(graph: &Path) -> PathBuf {
        graph.with_extension("storage-budget.lock")
    }

    pub fn acquire(graph: &Path) -> Result<Self, String> {
        let path = Self::path_for(graph);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("cannot open Last.fm storage budget lock: {error}"))?;
        file.lock_exclusive()
            .map_err(|error| format!("cannot lock Last.fm storage budget: {error}"))?;
        Ok(Self(file))
    }
}

impl Drop for BudgetLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
