//! Gateway → system-backend identity forwarding. A system backend (the workspace
//! server) never re-derives who is calling: the gateway already authenticated
//! the agent token and resolved the user from the flow record, so it forwards
//! that decision in one HMAC-signed header. Loopback-only transport plus this
//! signature is what lets the backend skip its own auth without trusting the
//! agent container, which shares the network namespace it could otherwise spoof
//! a plain header from.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

/// Header the gateway stamps on every request to a system backend (never a
/// third-party one — see `protocol.rs::inject_identity`). Never forward this
/// header's value from anywhere it might have been logged: treat it like a
/// bearer credential for the caller's identity.
pub const IDENTITY_HEADER: &str = "x-nasiko-identity";
const TTL_SECS: u64 = 60;

/// The caller identity a system backend trusts without re-deriving it. Built
/// once per request by the gateway (`SignedIdentity::new`) from state it
/// already resolved — an agent's authenticated token and, when the call
/// belongs to a live flow, that flow's `flows.user_id` — then signed
/// (`sign`) and read back (`verify`) by the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedIdentity {
    /// The agent container that made this call, resolved from its gateway
    /// bearer token.
    pub agent_id: Uuid,
    /// The human on whose behalf `agent_id` is acting — the flow's user when
    /// the call belongs to one, else (read-only methods outside any flow)
    /// the agent's owner.
    pub user_id: Uuid,
    /// The W3C trace id of the live flow this call belongs to, when there is
    /// one — `None` for a call resolved with no flow (e.g. the gateway's
    /// owner-fallback path for `initialize`/`tools/list`). Must come only
    /// from a flow the gateway itself verified against `flows`/
    /// `flow_participants`; a raw, unverified `traceparent` value is never an
    /// acceptable source (see `oss/server/src/mcp/handlers/gateway.rs::flow_user`).
    pub flow_id: Option<String>,
    /// Unix seconds after which `verify` rejects this identity, however
    /// correctly it's signed — bounds how long a captured header (unlikely,
    /// loopback-only) stays useful.
    pub exp: u64,
}

/// `None` only if the system clock reads earlier than the Unix epoch —
/// practically never happens, but `verify` must not treat that as "the
/// current time is 0": every real `exp` would then look unexpired forever,
/// which is failing *open* on a broken clock. Callers map `None` to
/// `Expired` instead (fail closed) rather than defaulting to a sentinel time.
fn now_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Why [`SignedIdentity::verify`] rejected a header — the MAC is checked
/// before any parsing (see `verify`'s own doc comment), so a variant here
/// reflects exactly what was found bad, in the order it's actually checked.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityError {
    /// Not `<payload>.<hex signature>`, non-hex signature, non-base64
    /// payload, or a payload whose decoded bytes aren't the expected JSON
    /// shape. Bucketed together deliberately: a caller reacting to this
    /// error should not be able to distinguish "your hex was invalid" from
    /// "your JSON was invalid" — both are just "not a well-formed identity".
    #[error("malformed identity header")]
    Malformed,
    /// The HMAC doesn't match under this key — tampering, or a key mismatch
    /// (e.g. verifying against the wrong deployment's
    /// `MCP_IDENTITY_SIGNING_KEY`). Checked before any parsing, so this can
    /// fire on a payload whose JSON shape was never even inspected.
    #[error("identity signature mismatch")]
    BadSignature,
    /// Signature verified, but `exp` has passed (or the clock read as
    /// earlier than the Unix epoch — treated the same as expired, never as a
    /// pass, since `now_secs()` can't produce a value to compare against).
    #[error("identity expired")]
    Expired,
}

impl SignedIdentity {
    /// Builds a fresh identity, `exp`-stamped [`TTL_SECS`] from now. Callers
    /// must pass only a `flow_id` they have themselves verified — see this
    /// struct's own `flow_id` doc — never a raw, unverified trace id.
    pub fn new(agent_id: Uuid, user_id: Uuid, flow_id: Option<String>) -> Self {
        Self {
            agent_id,
            user_id,
            flow_id,
            // A clock error here (see `now_secs`) produces an `exp` in the
            // distant past instead of the distant future — the identity
            // comes out already-expired rather than silently immortal.
            exp: now_secs().unwrap_or(0) + TTL_SECS,
        }
    }

    /// `<b64url(json)>.<hex(hmac-sha256)>`
    pub fn sign(&self, key: &[u8]) -> String {
        let payload = B64.encode(serde_json::to_vec(self).expect("identity serializes"));
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
        mac.update(payload.as_bytes());
        format!("{payload}.{}", hex::encode(mac.finalize().into_bytes()))
    }

    /// Verifies the MAC before parsing anything the payload claims —
    /// `BadSignature`/`Malformed` on the raw wire bytes are decided first, so
    /// a forged-but-well-formed payload never reaches `serde_json::from_slice`
    /// under a key that didn't sign it.
    pub fn verify(header: &str, key: &[u8]) -> Result<Self, IdentityError> {
        let (payload, sig_hex) = header.split_once('.').ok_or(IdentityError::Malformed)?;
        let sig = hex::decode(sig_hex).map_err(|_| IdentityError::Malformed)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
        mac.update(payload.as_bytes());
        mac.verify_slice(&sig)
            .map_err(|_| IdentityError::BadSignature)?;
        let bytes = B64.decode(payload).map_err(|_| IdentityError::Malformed)?;
        let id: Self = serde_json::from_slice(&bytes).map_err(|_| IdentityError::Malformed)?;
        // An unreadable clock must fail closed, not open: it must not read
        // as "the current time is 0", which would make every `exp` compare
        // as unexpired and disable this check entirely.
        let Some(now) = now_secs() else {
            return Err(IdentityError::Expired);
        };
        if id.exp < now {
            return Err(IdentityError::Expired);
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(key: &[u8]) -> (SignedIdentity, String) {
        let id = SignedIdentity {
            agent_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            flow_id: Some("abc".into()),
            exp: now_secs().unwrap() + 60,
        };
        let header = id.sign(key);
        (id, header)
    }

    #[test]
    fn roundtrip_verifies() {
        let key = b"k".to_vec();
        let (id, header) = sample(&key);
        assert_eq!(SignedIdentity::verify(&header, &key).unwrap(), id);
    }

    #[test]
    fn wrong_key_is_bad_signature() {
        let key = b"k".to_vec();
        let (_, header) = sample(&key);
        assert!(matches!(
            SignedIdentity::verify(&header, b"other"),
            Err(IdentityError::BadSignature)
        ));
    }

    #[test]
    fn tampered_signature_is_bad_signature() {
        let key = b"k".to_vec();
        let (_, header) = sample(&key);
        let mut broken = header.clone();
        // Flip a hex digit in the signature half, past the payload/'.' —
        // corrupting the payload instead would hit a different, already
        // separately-covered case (bad base64 / bad JSON). Pick the
        // replacement so it's guaranteed to differ from the original digit,
        // rather than relying on a 1/16 chance a fixed replacement lands on
        // a different character.
        let dot = broken.find('.').expect("well-formed header has a dot");
        let sig_start = dot + 1;
        let original = broken[sig_start..sig_start + 1].chars().next().unwrap();
        let replacement = if original == '0' { '1' } else { '0' };
        broken.replace_range(sig_start..sig_start + 1, &replacement.to_string());
        assert_ne!(broken, header);
        assert!(matches!(
            SignedIdentity::verify(&broken, &key),
            Err(IdentityError::BadSignature)
        ));
    }

    #[test]
    fn missing_dot_is_malformed() {
        let key = b"k".to_vec();
        assert!(matches!(
            SignedIdentity::verify("no-dot-in-here", &key),
            Err(IdentityError::Malformed)
        ));
    }

    #[test]
    fn non_hex_signature_is_malformed() {
        let key = b"k".to_vec();
        assert!(matches!(
            SignedIdentity::verify("cGF5bG9hZA.not-hex-zz", &key),
            Err(IdentityError::Malformed)
        ));
    }

    #[test]
    fn truncated_signature_is_bad_signature() {
        let key = b"k".to_vec();
        let (_, header) = sample(&key);
        let dot = header.find('.').unwrap();
        // A syntactically valid (even-length, all-hex) but short signature:
        // `hex::decode` succeeds, so this exercises `verify_slice`'s length
        // check, not `hex::decode`'s — a 4-byte MAC can never equal the
        // real 32-byte HMAC-SHA256 output, so this is deterministically a
        // signature mismatch, never a parse failure.
        let truncated = format!("{}.{}", &header[..dot], &header[dot + 1..dot + 9]);
        assert!(matches!(
            SignedIdentity::verify(&truncated, &key),
            Err(IdentityError::BadSignature)
        ));
    }

    #[test]
    fn bad_base64_payload_under_a_valid_mac_is_malformed() {
        // The MAC must be checked against the actual header bytes, not a
        // re-derivation, so a garbage-but-freshly-signed payload proves the
        // base64-decode failure is what's actually reported, with a MAC that
        // genuinely verifies against this exact (malformed) payload string.
        let key = b"k".to_vec();
        let payload = "not valid base64url!!";
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
        mac.update(payload.as_bytes());
        let header = format!("{payload}.{}", hex::encode(mac.finalize().into_bytes()));
        assert!(matches!(
            SignedIdentity::verify(&header, &key),
            Err(IdentityError::Malformed)
        ));
    }

    #[test]
    fn bad_json_payload_under_a_valid_mac_is_malformed() {
        // Same idea, one layer in: valid base64, but the decoded bytes
        // aren't the expected JSON shape.
        let key = b"k".to_vec();
        let payload = B64.encode(b"not the identity struct");
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
        mac.update(payload.as_bytes());
        let header = format!("{payload}.{}", hex::encode(mac.finalize().into_bytes()));
        assert!(matches!(
            SignedIdentity::verify(&header, &key),
            Err(IdentityError::Malformed)
        ));
    }

    #[test]
    fn expired_fails() {
        let key = b"k".to_vec();
        let id = SignedIdentity {
            agent_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            flow_id: None,
            exp: now_secs().unwrap() - 1,
        };
        assert!(matches!(
            SignedIdentity::verify(&id.sign(&key), &key),
            Err(IdentityError::Expired)
        ));
    }
}
