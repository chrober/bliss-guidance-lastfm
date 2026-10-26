# Retained Last.fm Data Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enforce the 80 MiB safety budget against a deterministic accounting of retained Last.fm response fields while reporting physical SQLite storage separately.

**Architecture:** Schema v2 is an artist-only SQLite graph and stores one conservative canonical-payload byte count per artist observation. The repository derives that count from the normalized rows actually retained, backfills existing v1 artist observations, removes former track tables, and exposes an aggregate. Track similarities remain job-local or in a separate short-lived JSON cache. Maintenance uses the aggregate plus any still-present legacy Last.fm cache for the budget, while `database_bytes` remains an operational storage metric.

**Tech Stack:** Rust, rusqlite, serde_json, Perl/Lyrion settings UI.

**Spec:** `D:/LMS/bliss-similarity-design/LASTFM_OFFLINE_GUIDANCE_GRAPH_DESIGN.md`

## Global Constraints

- Keep the contractual ceiling at 100 MB and the internal safety stop at 80 MiB.
- Count retained Last.fm result identities, ranks, scores and HTTP validators; do not count Lyrion roots, local timestamps, SQLite pages, indexes, WAL or SHM.
- Keep physical database size visible as a separate operational value.
- Existing v1 databases must preserve artist observations while removing persisted track observations.
- API keys and raw response bodies must not be persisted.

## Review Focus

- A v1 graph containing observations migrates and receives non-zero accounting without losing edges.
- Revalidation via HTTP 304 preserves retained-data bytes.
- Replacing an observation updates rather than accumulates its byte count.
- Local-only scope counts only result rows that are retained.
- A large WAL never exhausts the Last.fm-data budget.

---

### Task 1: Persist deterministic retained-data accounting

**Files:**
- Modify: `src/store/schema.rs`
- Modify: `src/store/repository.rs`
- Modify: `src/store/mod.rs`
- Test: `tests/store_repository.rs`

**Interfaces:**
- Produces: `GraphStore::retained_lastfm_bytes() -> Result<u64, String>` and schema v2 query columns.

- [x] Write failing schema-migration, replacement, revalidation and aggregate tests.
- [x] Run the focused store tests and confirm failures are caused by the absent schema/API.
- [x] Add schema v2 migration and canonical retained-row serialization/accounting.
- [x] Run the focused tests and the complete native suite.

### Task 2: Apply logical budget and expose both measurements

**Files:**
- Modify: `src/maintenance.rs`
- Modify: `src/progress.rs`
- Test: `tests/maintenance_cli.rs`

**Interfaces:**
- Consumes: `GraphStore::retained_lastfm_bytes()`.
- Produces: `retained_lastfm_bytes` in status/progress while preserving `database_bytes`.

- [x] Write failing tests proving WAL size is excluded and logical retained bytes drive exhaustion.
- [x] Run the focused maintenance tests and confirm the expected failures.
- [x] Replace physical-file budget checks with retained-data checks plus any legacy cache bytes.
- [x] Run focused and complete native verification, formatting and Clippy.

### Task 3: Render unambiguous provider status

**Files:**
- Modify: `D:/LMS/lms-guidance-lastfm/LastFmGuidance/Plugin.pm`
- Modify: `D:/LMS/lms-guidance-lastfm/LastFmGuidance/HTML/EN/plugins/LastFmGuidance/settings/lastfmguidance.html`
- Modify: `D:/LMS/lms-guidance-lastfm/LastFmGuidance/strings.txt`
- Modify: `D:/LMS/lms-guidance-lastfm/tests/plugin_contract.t`
- Modify: `D:/LMS/lms-guidance-lastfm/tests/settings_contract.t`

**Interfaces:**
- Consumes: native `database_bytes` and `retained_lastfm_bytes` fields.
- Produces: distinct SQLite-storage and retained-Last.fm-data values in initial and live status.

- [x] Write failing plugin/template contract tests for both values and their labels.
- [x] Run the focused Perl tests and confirm the expected failures.
- [x] Forward and render both fields without changing maintenance controls.
- [x] Run the complete plugin test suite and repository checks.

### Task 4: Align product documentation

**Files:**
- Modify: `D:/LMS/bliss-similarity-design/LASTFM_OFFLINE_GUIDANCE_GRAPH_DESIGN.md`
- Modify: `README.md`
- Modify: `D:/LMS/lms-guidance-lastfm/README.md`

- [x] Replace physical-file quota wording with the approved canonical retained-data accounting.
- [x] Document schema migration, the two displayed measurements and the conservative duplicate counting.
- [x] Verify links, terminology and Markdown formatting.

### Task 5: Final verification

- [x] Run all native Rust gates and all Perl/Python plugin gates.
- [x] Inspect the complete diffs for secrets, raw Last.fm payload persistence, generated binaries and unrelated files.
- [x] Perform a final self-review because subagent delegation is not enabled for this task.
