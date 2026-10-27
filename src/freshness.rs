// SPDX-License-Identifier: GPL-3.0-only

use sha2::{Digest, Sha256};
use std::time::UNIX_EPOCH;

pub const ARTIST_POSITIVE_FALLBACK_SECONDS: i64 = 30 * 86_400;
pub const ARTIST_EMPTY_FALLBACK_SECONDS: i64 = 7 * 86_400;
pub const TRACK_FALLBACK_SECONDS: i64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreshnessClass {
    ArtistPositive,
    ArtistEmpty,
    Track,
}

pub fn artist_observation_is_fresh(
    now: i64,
    validated_at: i64,
    expires_at: i64,
    refresh_days: Option<u32>,
) -> bool {
    match refresh_days {
        Some(days) => validated_at.saturating_add(i64::from(days) * 86_400) > now,
        None => expires_at > now,
    }
}

pub fn expires_at(
    now: i64,
    cache_control: Option<&str>,
    expires: Option<&str>,
    class: FreshnessClass,
) -> i64 {
    if let Some(seconds) = cache_control.and_then(max_age) {
        return now.saturating_add(seconds);
    }
    if let Some(timestamp) = expires.and_then(http_timestamp) {
        if timestamp > now {
            return timestamp;
        }
    }
    now.saturating_add(match class {
        FreshnessClass::ArtistPositive => ARTIST_POSITIVE_FALLBACK_SECONDS,
        FreshnessClass::ArtistEmpty => ARTIST_EMPTY_FALLBACK_SECONDS,
        FreshnessClass::Track => TRACK_FALLBACK_SECONDS,
    })
}

fn max_age(value: &str) -> Option<i64> {
    value.split(',').map(str::trim).find_map(|directive| {
        let (name, value) = directive.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("max-age")
            .then(|| value.trim().trim_matches('"').parse::<i64>().ok())
            .flatten()
            .filter(|value| *value >= 0)
    })
}

fn http_timestamp(value: &str) -> Option<i64> {
    httpdate::parse_http_date(value)
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

pub fn canonical_artist_payload_hash(
    method: &str,
    source_identity: &str,
    results: &[(&str, &str, u32, Option<f64>)],
) -> String {
    let mut digest = Sha256::new();
    write_field(&mut digest, method);
    write_field(&mut digest, source_identity);
    for (target_identity, target_name, rank, score) in results {
        write_field(&mut digest, target_identity);
        write_field(&mut digest, target_name);
        digest.update(rank.to_be_bytes());
        digest.update(
            score
                .unwrap_or(-1.0)
                .clamp(-1.0, 1.0)
                .to_bits()
                .to_be_bytes(),
        );
    }
    format!("{:x}", digest.finalize())
}

fn write_field(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}
