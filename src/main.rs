// SPDX-License-Identifier: GPL-3.0-only

use bliss_playlist_guidance_spi::{
    encode, Candidate, Capability, Diagnostics, GuidanceRequest, GuidanceResponse, GuidanceScope,
    GuidanceSignal, Manifest, PROTOCOL_NAME, SPI_VERSION,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};

const PROVIDER_ID: &str = "lastfm-guidance";
const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct SemanticArtifact {
    schema_version: u8,
    edges: Vec<SemanticEdge>,
}

#[derive(Debug, Deserialize)]
struct SemanticEntity {
    kind: String,
    id: String,
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
    // Source entity id -> candidate id -> (support, confidence, rationale).
    edges: HashMap<String, HashMap<String, EdgeScore>>,
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
            capabilities: vec![Capability::EdgeCandidateGuidance],
            required_context: vec!["candidate_identity".to_owned(), "route_context".to_owned()],
            configuration_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_path": {"type": "string", "minLength": 1}
                },
                "required": ["artifact_path"],
                "additionalProperties": false
            })),
        }
    }

    fn prepare(&mut self, options: &Value) -> Result<(Option<String>, Diagnostics), String> {
        let artifact_path = options
            .get("artifact_path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "options.artifact_path is required".to_owned())?;
        let bytes = fs::read(artifact_path)
            .map_err(|error| format!("cannot read semantic artifact: {error}"))?;
        let artifact: SemanticArtifact = serde_json::from_slice(&bytes)
            .map_err(|error| format!("cannot decode semantic artifact: {error}"))?;
        if artifact.schema_version != 1 {
            return Err("unsupported semantic artifact schema".to_owned());
        }

        self.edges.clear();
        for edge in artifact.edges {
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
            let entry = self
                .edges
                .entry(edge.source.id.clone())
                .or_default()
                .entry(candidate_id)
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

        let snapshot_id = format!("{}:sources:{}", artifact_path, self.edges.len());
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
                    "resolved_candidate_edges": self.edges.values().map(HashMap::len).sum::<usize>(),
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
        let source_ids: Vec<&str> = [
            context.left_anchor_id.as_deref(),
            context.right_anchor_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let signals: Vec<GuidanceSignal> = candidates
            .iter()
            .filter_map(|candidate| {
                let mut best: Option<(f64, f64, String, Option<String>)> = None;
                for source_id in &source_ids {
                    let Some(score) = self
                        .edges
                        .get(*source_id)
                        .and_then(|by_candidate| by_candidate.get(&candidate.candidate_id))
                    else {
                        continue;
                    };
                    let weighted = score.score * score.confidence;
                    if best
                        .as_ref()
                        .map(|entry| weighted > entry.0)
                        .unwrap_or(true)
                    {
                        best = Some((
                            weighted,
                            score.confidence,
                            score.kind.clone(),
                            score.observed_at.clone(),
                        ));
                    }
                }
                let (score, confidence, kind, observed_at) = best?;
                Some(
                    GuidanceSignal {
                        candidate_id: candidate.candidate_id.clone(),
                        scope: GuidanceScope::Edge,
                        score: score.clamp(0.0, 1.0),
                        confidence: confidence.clamp(0.0, 1.0),
                        rationale: Some(format!("Last.fm similar {kind}")),
                        observed_at,
                    }
                    .bounded(),
                )
            })
            .collect();
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
            options,
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
            match provider.prepare(&options) {
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
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "bliss-guidance-lastfm-{}-{}.json",
            std::process::id(),
            PROVIDER_VERSION.replace('.', "-")
        ))
    }

    #[test]
    fn manifest_identifies_guidance_provider() {
        let manifest = Provider::manifest();
        assert_eq!(manifest.provider_id, "lastfm-guidance");
        assert_eq!(manifest.protocol, PROTOCOL_NAME);
        assert_eq!(
            manifest.capabilities,
            vec![Capability::EdgeCandidateGuidance]
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
        let options = serde_json::json!({"artifact_path": path});
        provider.prepare(&options).unwrap();
        let context = bliss_playlist_guidance_spi::ScoreContext {
            scope: GuidanceScope::Edge,
            left_anchor_id: Some("missing-source".to_owned()),
            right_anchor_id: Some("source-a".to_owned()),
            context_track_ids: vec![],
        };
        let candidates = vec![Candidate {
            candidate_id: "candidate-1".to_owned(),
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
}
