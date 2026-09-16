// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Pure CDGC helpers (no PDK imports) — request-target encoding, nonce, and the
//! cached-contract types. The HTTP fetch lives in lib.rs (needs the HttpClient).

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::conformance::ContractField;

/// Cached, parsed field contract for one asset.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CachedContract {
    pub fields: Vec<ContractField>,
    /// Unix seconds when fetched — drives the refresh TTL.
    pub timestamp: i64,
}

/// Single-initiator refresh lock entry (stampede control).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RefreshLock {
    pub acquired_at: i64,
}

/// Percent-encode a value for safe interpolation into a request path segment.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Per-request JWT nonce: nanoseconds since the Unix epoch as a decimal string.
pub fn nonce_from_time(now: SystemTime) -> String {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encodes() {
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
        assert_eq!(percent_encode("BT-47_x.y~z"), "BT-47_x.y~z");
    }
}
