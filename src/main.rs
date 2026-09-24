// SPDX-License-Identifier: GPL-3.0-only

use bliss_playlist_guidance_spi::{
    encode, ArtifactDescriptor, Candidate, Capability, ChannelDescriptor, Diagnostics,
    GuidanceRequest, GuidanceResponse, GuidanceScope, GuidanceSignal, Manifest, PROTOCOL_NAME,
    SPI_VERSION,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, BufRead, Write};

const PROVIDER_ID: &str = "lastfm-guidance";
const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROGRAM: &str = env!("CARGO_PKG_NAME");

fn version_metadata_json() -> String {
    format!(
        "{{\"schema_version\":1,\"program\":\"{PROGRAM}\",\"version\":\"{PROVIDER_VERSION}\",\"provider_id\":\"{PROVIDER_ID}\",\"spi_version\":{SPI_VERSION}}}"
    )
}

fn usage() -> &'static str {
    "Usage:\n  bliss-guidance-lastfm version [--json]\n  bliss-guidance-lastfm"
}

#[derive(Debug, Deserialize)]
struct SemanticArtifact {
    schema_version: u8,
    edges: Vec<SemanticEdge>,
}

#[derive(Debug, Deserialize)]
struct SemanticEntity {
    kind: String,
    id: String,
    #[serde(default)]
    mbid: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SemanticEdge {
    source: SemanticEntity,
    #[serde(default)]
    resolved_candidate_id: Option<String>,
    #[serde(default)]
    raw_rank: Option<u32>,
    #[serde(default)]
    raw_score: Option<f64>,
    #[serde(default = "default_confidence")]
    identity_confidence: f64,
    #[serde(default)]
    observed_at: Option<String>,
}

fn default_confidence() -> f64 {
    1.0
}

#[derive(Default)]
struct Provider {
    // Source entity id -> candidate id -> channel -> strongest relation.
    edges: HashMap<String, HashMap<String, BTreeMap<String, EdgeScore>>>,
    // LMS source-track ID -> artist source IDs used by Last.fm artist edges.
    // This bridge is derived from the host-provided SPI anchors at prepare
    // time, preferably via MusicBrainz artist IDs.
    artist_sources_by_track: HashMap<String, Vec<String>>,
    snapshot_id: Option<String>,
    prepared: bool,
}

#[derive(Clone)]
struct EdgeScore {
    score: f64,
    confidence: f64,
    kind: String,
    observed_at: Option<String>,
}

impl Provider {
    fn manifest() -> Manifest {
        Manifest {
            spi_version: SPI_VERSION,
            provider_id: PROVIDER_ID.to_owned(),
            provider_version: PROVIDER_VERSION.to_owned(),
            protocol: PROTOCOL_NAME.to_owned(),
            capabilities: vec![
                Capability::GlobalCandidateGuidance,
                Capability::EdgeCandidateGuidance,
            ],
            channels: vec![
                ChannelDescriptor {
                    channel: "lastfm_track".to_owned(),
                    scopes: vec![GuidanceScope::Edge, GuidanceScope::Global],
                },
                ChannelDescriptor {
                    channel: "lastfm_artist".to_owned(),
                    scopes: vec![GuidanceScope::Edge, GuidanceScope::Global],
                },
            ],
            required_context: vec!["candidate_identity".to_owned(), "route_context".to_owned()],
            configuration_schema: Some(serde_json::json!({
                "type": "object",
                "additionalProperties": false
            })),
        }
    }

    fn prepare_with_anchors(
        &mut self,
        artifacts: &[ArtifactDescriptor],
        _resources: &[bliss_playlist_guidance_spi::ResourceDescriptor],
        anchors: &[bliss_playlist_guidance_spi::Anchor],
    ) -> Result<(Option<String>, Diagnostics), String> {
        let descriptor = artifacts
            .iter()
            .find(|artifact| artifact.kind == "resolved-lastfm-evidence-v1")
            .ok_or_else(|| "resolved-lastfm-evidence-v1 artifact is required".to_owned())?;
        let bytes = fs::read(&descriptor.path)
            .map_err(|error| format!("cannot read semantic artifact: {error}"))?;
        let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
        if !actual_sha256.eq_ignore_ascii_case(&descriptor.sha256) {
            return Err("semantic artifact sha256 mismatch".to_owned());
        }
        let artifact: SemanticArtifact = serde_json::from_slice(&bytes)
            .map_err(|error| format!("cannot decode semantic artifact: {error}"))?;
        if artifact.schema_version != 1 {
            return Err("unsupported semantic artifact schema".to_owned());
        }

        self.edges.clear();
        self.artist_sources_by_track.clear();
        let mut artist_sources_by_mbid = HashMap::<String, BTreeSet<String>>::new();
        let mut artist_sources_by_name = HashMap::<String, BTreeSet<String>>::new();
        for edge in &artifact.edges {
            if edge.source.kind != "artist" {
                continue;
            }
            if let Some(mbid) = edge.source.mbid.as_deref() {
                artist_sources_by_mbid
                    .entry(mbid.to_ascii_lowercase())
                    .or_default()
                    .insert(edge.source.id.clone());
            }
            if let Some(name) = edge.source.name.as_deref() {
                artist_sources_by_name
                    .entry(normalize_artist_name(name))
                    .or_default()
                    .insert(edge.source.id.clone());
            }
        }
        for anchor in anchors {
            let mut artist_sources = BTreeSet::new();
            for mbid in &anchor.track.artist_mbids {
                if let Some(source_ids) = artist_sources_by_mbid.get(&mbid.to_ascii_lowercase()) {
                    artist_sources.extend(source_ids.iter().cloned());
                }
            }
            // MBIDs are the authoritative join. Name matching keeps artist
            // guidance useful for a source whose metadata has no artist MBID.
            if artist_sources.is_empty() {
                if let Some(name) = anchor.track.artist.as_deref() {
                    if let Some(source_ids) =
                        artist_sources_by_name.get(&normalize_artist_name(name))
                    {
                        artist_sources.extend(source_ids.iter().cloned());
                    }
                }
            }
            if !artist_sources.is_empty() {
                self.artist_sources_by_track.insert(
                    anchor.anchor_id.clone(),
                    artist_sources.into_iter().collect(),
                );
            }
        }
        for edge in artifact.edges {
            let channel = match edge.source.kind.as_str() {
                // Better Call Bliss serializes Last.fm track observations as
                // recording entities, matching the shared semantic-evidence
                // vocabulary. Retain `track` for older valid artifacts.
                "recording" | "track" => "lastfm_track",
                "artist" => "lastfm_artist",
                _ => continue,
            };
            let Some(candidate_id) = edge.resolved_candidate_id else {
                // Unresolved provider identities are deliberately ignored. The
                // optimizer must only receive guidance for local candidates.
                continue;
            };
            let score = edge
                .raw_score
                .map(|value| value.clamp(0.0, 1.0))
                .or_else(|| edge.raw_rank.map(|rank| 1.0 / f64::from(rank)))
                .unwrap_or(0.0);
            let by_channel = self
                .edges
                .entry(edge.source.id.clone())
                .or_default()
                .entry(candidate_id)
                .or_default();
            let entry = by_channel
                .entry(channel.to_owned())
                .or_insert_with(|| EdgeScore {
                    score: 0.0,
                    confidence: 0.0,
                    kind: edge.source.kind.clone(),
                    observed_at: edge.observed_at.clone(),
                });
            // Multiple source relationships support the same candidate. Keep
            // the strongest one for deterministic, bounded guidance.
            if score * edge.identity_confidence > entry.score * entry.confidence {
                entry.score = score;
                entry.confidence = edge.identity_confidence.clamp(0.0, 1.0);
                entry.kind = edge.source.kind;
                entry.observed_at = edge.observed_at;
            }
        }

        let snapshot_id = format!("{}:sources:{}", actual_sha256, self.edges.len());
        self.snapshot_id = Some(snapshot_id.clone());
        self.prepared = true;
        Ok((
            Some(snapshot_id),
            Diagnostics {
                state: Some("fresh".to_owned()),
                request_count: 1,
                failure_count: 0,
                details: Some(serde_json::json!({
                    "source_entities": self.edges.len(),
                    "track_artist_mappings": self.artist_sources_by_track.len(),
                    "resolved_candidate_edges": self.edges.values()
                        .map(|by_candidate| by_candidate.values().map(BTreeMap::len).sum::<usize>())
                        .sum::<usize>(),
                })),
            },
        ))
    }

    fn score(
        &self,
        request_id: &str,
        context: &bliss_playlist_guidance_spi::ScoreContext,
        candidates: &[Candidate],
    ) -> GuidanceResponse {
        if !self.prepared {
            return GuidanceResponse::Error {
                provider_id: Some(PROVIDER_ID.to_owned()),
                code: "NOT_PREPARED".to_owned(),
                message: "provider must receive prepare before score".to_owned(),
                retryable: false,
            };
        }
        let mut source_ids = context.context_track_ids.clone();
        source_ids.extend(
            [
                context.left_anchor_id.as_deref(),
                context.right_anchor_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(str::to_owned),
        );
        let artist_source_ids = source_ids
            .iter()
            .filter_map(|track_id| self.artist_sources_by_track.get(track_id))
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        source_ids.extend(artist_source_ids);
        source_ids.sort_unstable();
        source_ids.dedup();
        let mut signals = Vec::new();
        for candidate in candidates {
            let mut best_by_channel: BTreeMap<String, (f64, f64, String, Option<String>)> =
                BTreeMap::new();
            for source_id in &source_ids {
                let Some(by_channel) = self
                    .edges
                    .get(source_id)
                    .and_then(|by_candidate| by_candidate.get(&candidate.candidate_id))
                else {
                    continue;
                };
                for (channel, score) in by_channel {
                    let weighted = score.score * score.confidence;
                    let replace = best_by_channel
                        .get(channel)
                        .map(|entry| weighted > entry.0)
                        .unwrap_or(true);
                    if replace {
                        best_by_channel.insert(
                            channel.clone(),
                            (
                                weighted,
                                score.confidence,
                                score.kind.clone(),
                                score.observed_at.clone(),
                            ),
                        );
                    }
                }
            }
            for (channel, (score, confidence, kind, observed_at)) in best_by_channel {
                signals.push(
                    GuidanceSignal {
                        candidate_id: candidate.candidate_id.clone(),
                        channel,
                        scope: context.scope.clone(),
                        score: score.clamp(0.0, 1.0),
                        confidence: confidence.clamp(0.0, 1.0),
                        rationale: Some(format!("Last.fm similar {kind}")),
                        observed_at,
                    }
                    .bounded(),
                );
            }
        }
        let matched = signals.len();
        GuidanceResponse::Scores {
            provider_id: PROVIDER_ID.to_owned(),
            request_id: request_id.to_owned(),
            signals,
            diagnostics: Diagnostics {
                state: Some("fresh".to_owned()),
                request_count: 1,
                failure_count: 0,
                details: Some(serde_json::json!({"matched_candidates": matched})),
            },
        }
    }
}

fn normalize_artist_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn handle(provider: &mut Provider, request: GuidanceRequest) -> GuidanceResponse {
    match request {
        GuidanceRequest::Describe { spi_version } => {
            if spi_version != SPI_VERSION {
                return GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "UNSUPPORTED_SPI_VERSION".to_owned(),
                    message: format!("provider supports SPI version {SPI_VERSION}"),
                    retryable: false,
                };
            }
            GuidanceResponse::Manifest(Provider::manifest())
        }
        GuidanceRequest::Prepare {
            spi_version,
            artifacts,
            resources,
            anchors,
            ..
        } => {
            if spi_version != SPI_VERSION {
                return GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "UNSUPPORTED_SPI_VERSION".to_owned(),
                    message: format!("provider supports SPI version {SPI_VERSION}"),
                    retryable: false,
                };
            }
            match provider.prepare_with_anchors(&artifacts, &resources, &anchors) {
                Ok((snapshot_id, diagnostics)) => GuidanceResponse::Prepared {
                    provider_id: PROVIDER_ID.to_owned(),
                    snapshot_id,
                    diagnostics,
                },
                Err(message) => GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "PREPARE_FAILED".to_owned(),
                    message,
                    retryable: true,
                },
            }
        }
        GuidanceRequest::Score {
            spi_version,
            request_id,
            context,
            candidates,
        } => {
            if spi_version != SPI_VERSION {
                return GuidanceResponse::Error {
                    provider_id: Some(PROVIDER_ID.to_owned()),
                    code: "UNSUPPORTED_SPI_VERSION".to_owned(),
                    message: format!("provider supports SPI version {SPI_VERSION}"),
                    retryable: false,
                };
            }
            provider.score(&request_id, &context, &candidates)
        }
        GuidanceRequest::Close { .. } => GuidanceResponse::Closed {
            provider_id: PROVIDER_ID.to_owned(),
        },
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [command] if command == "version" => {
            println!("{PROGRAM} {PROVIDER_VERSION}");
            return;
        }
        [command, format] if command == "version" && format == "--json" => {
            println!("{}", version_metadata_json());
            return;
        }
        _ => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut provider = Provider::default();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) if !line.trim().is_empty() => line,
            Ok(_) => continue,
            Err(error) => {
                break_with_error(&mut stdout, "INPUT_FAILED", error.to_string());
                break;
            }
        };
        let response = match bliss_playlist_guidance_spi::decode_request(&line) {
            Ok(request) => handle(&mut provider, request),
            Err(error) => GuidanceResponse::Error {
                provider_id: Some(PROVIDER_ID.to_owned()),
                code: "INVALID_REQUEST".to_owned(),
                message: error.to_string(),
                retryable: false,
            },
        };
        if writeln!(stdout, "{}", encode(&response).unwrap()).is_err() {
            break;
        }
        if stdout.flush().is_err() {
            break;
        }
        if matches!(response, GuidanceResponse::Closed { .. }) {
            break;
        }
    }
}

fn break_with_error(stdout: &mut impl Write, code: &str, message: String) {
    let response = GuidanceResponse::Error {
        provider_id: Some(PROVIDER_ID.to_owned()),
        code: code.to_owned(),
        message,
        retryable: false,
    };
    let _ = writeln!(stdout, "{}", encode(&response).unwrap());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bliss_playlist_guidance_spi::ArtifactDescriptor;
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_SERIAL: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn version_metadata_identifies_lastfm_provider_and_spi() {
        let metadata = version_metadata_json();
        assert!(metadata.contains("\"program\":\"bliss-guidance-lastfm\""));
        assert!(metadata.contains("\"provider_id\":\"lastfm-guidance\""));
        assert!(metadata.contains("\"spi_version\":"));
    }

    fn fixture_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "bliss-guidance-lastfm-{}-{}-{}.json",
            std::process::id(),
            PROVIDER_VERSION.replace('.', "-"),
            FIXTURE_SERIAL.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn artifact_descriptor(path: &std::path::Path) -> ArtifactDescriptor {
        let bytes = fs::read(path).unwrap();
        ArtifactDescriptor {
            kind: "resolved-lastfm-evidence-v1".to_owned(),
            path: path.to_string_lossy().into_owned(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }

    #[test]
    fn manifest_identifies_guidance_provider() {
        let manifest = Provider::manifest();
        assert_eq!(manifest.provider_id, "lastfm-guidance");
        assert_eq!(manifest.protocol, PROTOCOL_NAME);
        assert_eq!(
            manifest.capabilities,
            vec![
                Capability::GlobalCandidateGuidance,
                Capability::EdgeCandidateGuidance,
            ]
        );
        assert_eq!(
            manifest
                .channels
                .iter()
                .map(|channel| channel.channel.as_str())
                .collect::<Vec<_>>(),
            vec!["lastfm_track", "lastfm_artist"]
        );
    }

    #[test]
    fn score_uses_either_available_anchor_and_ignores_unresolved_edges() {
        let path = fixture_path();
        let artifact = serde_json::json!({
            "schema_version": 1,
            "edges": [
                {
                    "source": {"kind": "track", "id": "source-a"},
                    "resolved_candidate_id": "candidate-1",
                    "raw_score": 0.9,
                    "identity_confidence": 0.8,
                    "observed_at": "2026-09-17T00:00:00Z"
                },
                {
                    "source": {"kind": "track", "id": "source-a"},
                    "raw_score": 1.0
                }
            ]
        });
        fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();

        let mut provider = Provider::default();
        let descriptor = artifact_descriptor(&path);
        provider
            .prepare_with_anchors(&[descriptor], &[], &[])
            .unwrap();
        let context = bliss_playlist_guidance_spi::ScoreContext {
            scope: GuidanceScope::Edge,
            left_anchor_id: Some("missing-source".to_owned()),
            right_anchor_id: Some("source-a".to_owned()),
            context_track_ids: vec![],
        };
        let candidates = vec![Candidate {
            candidate_id: "candidate-1".to_owned(),
            lms_urlmd5: None,
            database_file: None,
            title: None,
            artist: None,
            album: None,
            recording_mbid: None,
            artist_mbids: vec![],
        }];
        let response = provider.score("request-1", &context, &candidates);
        match response {
            GuidanceResponse::Scores { signals, .. } => {
                assert_eq!(signals.len(), 1);
                assert!((signals[0].score - 0.72).abs() < f64::EPSILON);
                assert!((signals[0].confidence - 0.8).abs() < f64::EPSILON);
            }
            other => panic!("expected scores response, got {other:?}"),
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn score_uses_resolved_recording_edges_written_by_better_call_bliss() {
        let path = fixture_path();
        let artifact = serde_json::json!({
            "schema_version": 1,
            "edges": [{
                "source": {"kind": "recording", "id": "lms-track-42"},
                "resolved_candidate_id": "bliss-row-7",
                "raw_score": 0.9,
                "identity_confidence": 1.0
            }]
        });
        fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();

        let mut provider = Provider::default();
        provider
            .prepare_with_anchors(&[artifact_descriptor(&path)], &[], &[])
            .unwrap();
        let response = provider.score(
            "recording-edge",
            &bliss_playlist_guidance_spi::ScoreContext {
                scope: GuidanceScope::Global,
                left_anchor_id: None,
                right_anchor_id: None,
                context_track_ids: vec!["lms-track-42".to_owned()],
            },
            &[Candidate {
                candidate_id: "bliss-row-7".to_owned(),
                lms_urlmd5: None,
                database_file: None,
                title: None,
                artist: None,
                album: None,
                recording_mbid: None,
                artist_mbids: vec![],
            }],
        );

        let GuidanceResponse::Scores { signals, .. } = response else {
            panic!("expected scores response");
        };
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].candidate_id, "bliss-row-7");
        assert_eq!(signals[0].channel, "lastfm_track");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn score_uses_global_context_track_ids_when_no_edge_anchor_is_set() {
        let path = fixture_path();
        let artifact = serde_json::json!({
            "schema_version": 1,
            "edges": [{
                "source": {"kind": "artist", "id": "source-b"},
                "resolved_candidate_id": "candidate-1",
                "raw_score": 0.8,
                "identity_confidence": 1.0
            }]
        });
        fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();

        let mut provider = Provider::default();
        provider
            .prepare_with_anchors(&[artifact_descriptor(&path)], &[], &[])
            .unwrap();
        let response = provider.score(
            "request-global",
            &bliss_playlist_guidance_spi::ScoreContext {
                scope: GuidanceScope::Global,
                left_anchor_id: None,
                right_anchor_id: None,
                context_track_ids: vec!["source-a".to_owned(), "source-b".to_owned()],
            },
            &[Candidate {
                candidate_id: "candidate-1".to_owned(),
                lms_urlmd5: None,
                database_file: None,
                title: None,
                artist: None,
                album: None,
                recording_mbid: None,
                artist_mbids: vec![],
            }],
        );

        let GuidanceResponse::Scores { signals, .. } = response else {
            panic!("expected scores response");
        };
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].channel, "lastfm_artist");
        assert_eq!(signals[0].scope, GuidanceScope::Global);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn score_expands_a_track_context_to_its_anchor_artist_mbid() {
        // Better Call Bliss asks global guidance with LMS track IDs, while its
        // Last.fm artist.getSimilar observations are keyed by artist IDs. The
        // provider must use the SPI prepare anchors to bridge those identities.
        let path = fixture_path();
        let artifact = serde_json::json!({
            "schema_version": 1,
            "edges": [{
                "source": {
                    "kind": "artist",
                    "id": "artist:layla-zoe",
                    "mbid": "artist-mbid-1",
                    "name": "Layla Zoe"
                },
                "resolved_candidate_id": "bliss-row-51431",
                "raw_score": 0.8,
                "identity_confidence": 1.0
            }]
        });
        fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();

        let anchor = bliss_playlist_guidance_spi::Anchor {
            anchor_id: "lms-track-2623395".to_owned(),
            track: Candidate {
                candidate_id: "lms-track-2623395".to_owned(),
                lms_urlmd5: None,
                database_file: None,
                title: None,
                artist: Some("Layla Zoe".to_owned()),
                album: None,
                recording_mbid: None,
                artist_mbids: vec!["artist-mbid-1".to_owned()],
            },
        };
        let mut provider = Provider::default();
        provider
            .prepare_with_anchors(&[artifact_descriptor(&path)], &[], &[anchor])
            .unwrap();

        let response = provider.score(
            "artist-via-track-context",
            &bliss_playlist_guidance_spi::ScoreContext {
                scope: GuidanceScope::Global,
                left_anchor_id: None,
                right_anchor_id: None,
                context_track_ids: vec!["lms-track-2623395".to_owned()],
            },
            &[Candidate {
                candidate_id: "bliss-row-51431".to_owned(),
                lms_urlmd5: None,
                database_file: None,
                title: None,
                artist: None,
                album: None,
                recording_mbid: None,
                artist_mbids: vec![],
            }],
        );

        let GuidanceResponse::Scores { signals, .. } = response else {
            panic!("expected scores response");
        };
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].channel, "lastfm_artist");
        assert_eq!(signals[0].candidate_id, "bliss-row-51431");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn score_expands_an_edge_anchor_to_its_artist_relation() {
        let path = fixture_path();
        let artifact = serde_json::json!({
            "schema_version": 1,
            "edges": [{
                "source": {
                    "kind": "artist",
                    "id": "artist:layla-zoe",
                    "mbid": "artist-mbid-1"
                },
                "resolved_candidate_id": "bliss-row-51431",
                "raw_score": 0.8
            }]
        });
        fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();
        let anchor = bliss_playlist_guidance_spi::Anchor {
            anchor_id: "lms-track-2623395".to_owned(),
            track: Candidate {
                candidate_id: "lms-track-2623395".to_owned(),
                lms_urlmd5: None,
                database_file: None,
                title: None,
                artist: Some("Layla Zoe".to_owned()),
                album: None,
                recording_mbid: None,
                artist_mbids: vec!["artist-mbid-1".to_owned()],
            },
        };
        let mut provider = Provider::default();
        provider
            .prepare_with_anchors(&[artifact_descriptor(&path)], &[], &[anchor])
            .unwrap();

        let response = provider.score(
            "artist-via-edge-anchor",
            &bliss_playlist_guidance_spi::ScoreContext {
                scope: GuidanceScope::Edge,
                left_anchor_id: Some("lms-track-2623395".to_owned()),
                right_anchor_id: None,
                context_track_ids: vec![],
            },
            &[Candidate {
                candidate_id: "bliss-row-51431".to_owned(),
                lms_urlmd5: None,
                database_file: None,
                title: None,
                artist: None,
                album: None,
                recording_mbid: None,
                artist_mbids: vec![],
            }],
        );

        let GuidanceResponse::Scores { signals, .. } = response else {
            panic!("expected scores response");
        };
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].channel, "lastfm_artist");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn prepare_rejects_a_lastfm_artifact_with_an_invalid_hash() {
        let path = fixture_path();
        fs::write(&path, br#"{"schema_version":1,"edges":[]}"#).unwrap();
        let mut descriptor = artifact_descriptor(&path);
        descriptor.sha256 = "0".repeat(64);

        let mut provider = Provider::default();
        let error = provider
            .prepare_with_anchors(&[descriptor], &[], &[])
            .unwrap_err();

        assert!(error.contains("sha256"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn score_preserves_independent_track_and_artist_channels() {
        let path = fixture_path();
        let artifact = serde_json::json!({
            "schema_version": 1,
            "edges": [
                {
                    "source": {"kind": "track", "id": "source-a"},
                    "resolved_candidate_id": "candidate-1",
                    "raw_score": 0.9,
                    "identity_confidence": 0.8
                },
                {
                    "source": {"kind": "artist", "id": "source-a"},
                    "resolved_candidate_id": "candidate-1",
                    "raw_score": 0.7,
                    "identity_confidence": 0.9
                }
            ]
        });
        fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();

        let mut provider = Provider::default();
        provider
            .prepare_with_anchors(&[artifact_descriptor(&path)], &[], &[])
            .unwrap();
        let response = provider.score(
            "request-1",
            &bliss_playlist_guidance_spi::ScoreContext {
                scope: GuidanceScope::Edge,
                left_anchor_id: Some("source-a".to_owned()),
                right_anchor_id: None,
                context_track_ids: vec![],
            },
            &[Candidate {
                candidate_id: "candidate-1".to_owned(),
                lms_urlmd5: None,
                database_file: None,
                title: None,
                artist: None,
                album: None,
                recording_mbid: None,
                artist_mbids: vec![],
            }],
        );

        let GuidanceResponse::Scores { signals, .. } = response else {
            panic!("expected scores response");
        };
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].channel, "lastfm_artist");
        assert_eq!(signals[1].channel, "lastfm_track");
        let _ = fs::remove_file(path);
    }
}
