// SPDX-License-Identifier: GPL-3.0-only

use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct GraphProgress {
    pub event_version: &'static str,
    pub phase: String,
    pub current: u64,
    pub total: u64,
    pub requests: u64,
    pub cached: u64,
    pub changed: u64,
    pub failures: u64,
    pub database_bytes: u64,
    pub message: String,
    pub finished: bool,
    pub cancelled: bool,
    pub paused: bool,
    pub retry_at: Option<i64>,
    pub budget_exhausted: bool,
}

impl GraphProgress {
    pub fn new(phase: &str, message: &str) -> Self {
        Self {
            event_version: "lastfm_graph_progress_v1",
            phase: phase.to_owned(),
            message: message.to_owned(),
            ..Self::default()
        }
    }
}
