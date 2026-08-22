//! Session affinity, so prompt caches actually hit.
//!
//! Every major provider scopes its prompt cache to the credential. Rotate
//! credentials between turns of a conversation and every turn is a cache miss:
//! on an agentic workload where the same system prompt and tool definitions
//! replay every turn, that is most of the bill.
//!
//! So the pool pins a conversation to a credential. The subtlety, learned by
//! sub2api the hard way, is *what to hash*: hashing the whole conversation
//! gives a different key every turn, which pins nothing. The key must be
//! derived from the part of the prompt that is **stable across turns** — which
//! is exactly the part the client marked cacheable.

use sha2::{Digest, Sha256};
use std::fmt;

/// A stable identifier for a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey(String);

impl SessionKey {
    /// Derive from the prompt blocks the client marked cacheable.
    ///
    /// This is the good case: the client set `cache_control` breakpoints, so we
    /// know precisely which prefix is meant to be stable and can key on exactly
    /// that. Returns `None` when nothing was marked, so the caller falls back
    /// rather than hashing an empty string and pinning every unmarked request
    /// in the fleet onto one credential.
    #[must_use]
    pub fn from_cache_blocks(blocks: &[&str]) -> Option<Self> {
        if blocks.is_empty() {
            return None;
        }
        let mut hasher = Sha256::new();
        for block in blocks {
            // Length-prefix so ["ab","c"] and ["a","bc"] cannot collide.
            hasher.update(block.len().to_le_bytes());
            hasher.update(block.as_bytes());
        }
        Some(Self(hex(&hasher.finalize())))
    }

    /// Derive from a session identifier the client supplied.
    ///
    /// Claude Code embeds one in `metadata.user_id`; other clients may send a
    /// header. Preferred over content hashing when present: it is exact, and it
    /// survives the conversation growing past its cache breakpoints.
    #[must_use]
    pub fn from_client_session(session: &str) -> Option<Self> {
        if session.is_empty() {
            return None;
        }
        let mut hasher = Sha256::new();
        hasher.update(b"client-session:");
        hasher.update(session.as_bytes());
        Some(Self(hex(&hasher.finalize())))
    }

    /// Last resort: hash whatever weakly identifies the caller.
    ///
    /// Deliberately coarse. It will not pin a conversation, but it keeps one
    /// API key's traffic from fanning across every credential at once, which
    /// still recovers some cache locality.
    #[must_use]
    pub fn from_caller(api_key_id: &str, model: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"caller:");
        hasher.update(api_key_id.as_bytes());
        hasher.update(b"|");
        hasher.update(model.as_bytes());
        Self(hex(&hasher.finalize()))
    }

    /// Resolve the best available key, in descending order of precision.
    #[must_use]
    pub fn resolve(
        client_session: Option<&str>,
        cache_blocks: &[&str],
        api_key_id: &str,
        model: &str,
    ) -> Self {
        client_session
            .and_then(Self::from_client_session)
            .or_else(|| Self::from_cache_blocks(cache_blocks))
            .unwrap_or_else(|| Self::from_caller(api_key_id, model))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The Redis key this session's pin lives under.
    #[must_use]
    pub fn redis_key(&self, route: &str) -> String {
        format!("oag:sticky:{route}:{}", self.0)
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_blocks_give_a_stable_key_across_turns() {
        // The whole point: the conversation grows, but the cacheable prefix —
        // system prompt and tool definitions — does not, so the key is stable
        // and the conversation stays pinned to one credential.
        let system = "You are a helpful assistant with these tools...";
        let turn_one = SessionKey::from_cache_blocks(&[system]).unwrap();
        let turn_seven = SessionKey::from_cache_blocks(&[system]).unwrap();
        assert_eq!(turn_one, turn_seven);
    }

    #[test]
    fn different_prefixes_land_on_different_keys() {
        let a = SessionKey::from_cache_blocks(&["prompt A"]).unwrap();
        let b = SessionKey::from_cache_blocks(&["prompt B"]).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn block_boundaries_are_not_collidable() {
        // Without length prefixing these two hash identically, and two
        // unrelated conversations would share a pin.
        let a = SessionKey::from_cache_blocks(&["ab", "c"]).unwrap();
        let b = SessionKey::from_cache_blocks(&["a", "bc"]).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn empty_input_does_not_produce_a_key() {
        // Otherwise every unmarked request in the fleet pins to one credential.
        assert!(SessionKey::from_cache_blocks(&[]).is_none());
        assert!(SessionKey::from_client_session("").is_none());
    }

    #[test]
    fn an_explicit_client_session_beats_content_hashing() {
        let by_session = SessionKey::resolve(Some("sess_abc"), &["some prompt"], "key1", "opus");
        let by_content = SessionKey::resolve(None, &["some prompt"], "key1", "opus");
        assert_ne!(by_session, by_content);
        assert_eq!(
            by_session,
            SessionKey::from_client_session("sess_abc").unwrap(),
            "an exact session id should win over inferring one from content"
        );
    }

    #[test]
    fn resolve_always_yields_something() {
        // No session, no cache blocks: still pinned, just coarsely.
        let k = SessionKey::resolve(None, &[], "key1", "opus");
        assert_eq!(k, SessionKey::from_caller("key1", "opus"));
    }
}
