//! Per-user bearer tokens for the MCP endpoint.
//!
//! Lives in `core` because two services touch it: the dashboard mints and
//! revokes tokens, and the MCP server checks them. Only a hash of each token
//! is ever stored, so the tokens index leaking — or the MCP service's search
//! key reading it — gives away nothing that can be presented as a credential.

use serde::{Deserialize, Serialize};

use crate::model::now_ts;

/// Every minted token starts with this, so one is recognisable in a config
/// file or a secret scanner, and so the MCP server can skip the lookup for
/// anything that is obviously not one.
pub const PREFIX: &str = "rgn_";

/// Hex characters after [`PREFIX`]: 32 random bytes.
pub const SECRET_HEX_LEN: usize = 64;

/// How much of the token the dashboard shows, so an operator can match a
/// row to the token someone is holding without the row being a credential.
const HINT_LEN: usize = PREFIX.len() + 8;

/// One minted token, as stored. The secret itself is never here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToken {
    /// [`hash`] of the full token. Doubles as the lookup key.
    pub id: String,
    /// Who or what this was issued to.
    pub name: String,
    /// The first few characters of the token, for recognising it.
    pub hint: String,
    pub created_at: i64,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub revoked_at: Option<i64>,
}

impl McpToken {
    /// A record for a freshly generated `token`.
    pub fn new(token: &str, name: String, expires_at: Option<i64>) -> Self {
        Self {
            id: hash(token),
            name,
            hint: token.chars().take(HINT_LEN).collect(),
            created_at: now_ts(),
            expires_at,
            revoked_at: None,
        }
    }

    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|t| t > now)
    }
}

/// The stored form of a token.
///
/// A fast unsalted hash is right here, unlike for passwords: the input is
/// 256 random bits, so there is nothing to brute-force.
pub fn hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// Is this shaped like a minted token? Cheap, and lets the MCP server
/// reject garbage without a database round trip.
pub fn looks_minted(token: &str) -> bool {
    token.strip_prefix(PREFIX).is_some_and(|s| {
        s.len() == SECRET_HEX_LEN && s.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// Build a token from 32 bytes the caller drew from a CSPRNG. `core` does
/// not pick the randomness source so it does not need to depend on one.
pub fn format(secret: [u8; 32]) -> String {
    let mut out = String::with_capacity(PREFIX.len() + SECRET_HEX_LEN);
    out.push_str(PREFIX);
    for b in secret {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatted_tokens_are_recognised() {
        let t = format([0xab; 32]);
        assert!(looks_minted(&t));
        assert!(!looks_minted(&t[..t.len() - 1]));
        assert!(!looks_minted(&t.replace(PREFIX, "xyz_")));
        assert!(!looks_minted(&format!("{PREFIX}{}", "g".repeat(SECRET_HEX_LEN))));
    }

    #[test]
    fn the_record_never_holds_the_secret() {
        let t = format([7; 32]);
        let rec = McpToken::new(&t, "someone".into(), None);
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains(&t[HINT_LEN..]));
        assert_eq!(rec.id, hash(&t));
        assert!(rec.hint.starts_with(PREFIX));
    }

    #[test]
    fn revoked_and_expired_tokens_are_inactive() {
        let t = format([1; 32]);
        let mut rec = McpToken::new(&t, "x".into(), Some(100));
        assert!(rec.is_active(99));
        assert!(!rec.is_active(100));
        rec.expires_at = None;
        assert!(rec.is_active(i64::MAX));
        rec.revoked_at = Some(1);
        assert!(!rec.is_active(0));
    }
}
