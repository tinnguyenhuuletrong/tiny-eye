//! Deterministic identities: series ids (SPEC §4.4) and log pattern hashes (SPEC §5.4).
//! Both must be identical on every node, in every region, after every restart.

use crate::tables::Labels;
use std::sync::LazyLock;

/// `xxh64` over length-prefixed parts, so no label value can collide with a separator.
/// Tenant and region come from the receiving node's config, never from the payload.
pub fn series_id(tenant: &str, region: &str, metric: &str, labels: &Labels) -> u64 {
    let mut buf = Vec::with_capacity(64 + labels.len() * 32);
    let mut part = |b: &[u8]| {
        buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
        buf.extend_from_slice(b);
    };
    part(tenant.as_bytes());
    part(region.as_bytes());
    part(metric.as_bytes());
    let mut canon = Vec::with_capacity(labels.len() * 32);
    for (k, v) in labels {
        canon.extend_from_slice(&(k.len() as u32).to_le_bytes());
        canon.extend_from_slice(k.as_bytes());
        canon.extend_from_slice(&(v.len() as u32).to_le_bytes());
        canon.extend_from_slice(v.as_bytes());
    }
    part(&canon);
    xxhash_rust::xxh64::xxh64(&buf, 0)
}

/// Version of the masking rules below; stored next to every hash.
pub const PATTERN_VERSION: u8 = 1;

static MASKS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    [
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}", // UUID
        r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b",                                        // IPv4[:port]
        r"\b[0-9a-fA-F]{8,}\b",                                                         // hex runs
        r#""[^"]*""#,                                                                   // quoted strings
        r"\d+(?:\.\d+)?",                                                               // numbers
    ]
    .iter()
    .map(|r| regex::Regex::new(r).expect("static regex"))
    .collect()
});

/// Fixed, versioned masking: variable parts become `<*>`.
pub fn mask(body: &str) -> String {
    MASKS.iter().fold(body.to_string(), |b, r| r.replace_all(&b, "<*>").into_owned())
}

pub fn pattern_hash(body: &str) -> u64 {
    xxhash_rust::xxh64::xxh64(mask(body).as_bytes(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_id_is_injective_on_separators() {
        let a: Labels = [("a".to_string(), "b,c=d".to_string())].into();
        let b: Labels = [("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string())].into();
        assert_ne!(series_id("t", "r", "m", &a), series_id("t", "r", "m", &b));
        assert_eq!(series_id("t", "r", "m", &a), series_id("t", "r", "m", &a.clone()));
    }

    #[test]
    fn masking_groups_templates() {
        assert_eq!(pattern_hash("user 12 placed order 99"), pattern_hash("user 7 placed order 123456"));
        assert_eq!(mask("connect 10.0.0.1:5432 id 550e8400-e29b-41d4-a716-446655440000"), "connect <*> id <*>");
        assert_ne!(pattern_hash("payment declined"), pattern_hash("payment accepted"));
    }
}
