//! Deterministic identifiers.
//!
//! Derived records get ids from their content rather than ULIDs, so deleting
//! the index and rebuilding it reproduces the same ids and the same query
//! results (milestone M0 acceptance).

use sha2::{Digest, Sha256};

/// 64-hex fingerprint of the parts, unambiguous across part boundaries.
pub fn fingerprint(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    hex::encode(h.finalize())
}

/// `prefix_` + the first 20 hex characters of a fingerprint.
pub fn from_fp(prefix: &str, fp: &str) -> String {
    format!("{prefix}_{}", &fp[..20])
}

pub fn stable_id(prefix: &str, parts: &[&str]) -> String {
    from_fp(prefix, &fingerprint(parts))
}
