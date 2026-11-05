//! What is stored for a task body or key-value value: the JSON exactly as the agent sent it,
//! size-checked, then sealed.

use nasiko_secrets::SecretsCrypto;
use serde_json::value::RawValue;
use uuid::Uuid;

use super::error::StateError;
use super::repo::Store;

/// A task body or value as received, already within the size cap.
///
/// The server never parses it into a JSON tree: the agent SDK's serialization is the contract,
/// so number precision, key order, escapes and whitespace all come back exactly as sent. The cap
/// is measured on those received bytes, which are also what gets stored.
pub(super) struct BoundedPayload<'a>(&'a str);

impl<'a> BoundedPayload<'a> {
    pub(super) fn new(raw: &'a RawValue, max_bytes: usize) -> Result<Self, StateError> {
        let text = raw.get();
        if text.len() > max_bytes {
            return Err(StateError::PayloadTooLarge(format!(
                "value is {} bytes as sent; the limit is {max_bytes}",
                text.len()
            )));
        }
        Ok(Self(text))
    }
}

/// The most a sealed payload's stored size exceeds 4/3 of the payload's own size.
///
/// `SecretsCrypto::encrypt_with_aad` stores `base64(12-byte nonce || ciphertext || 16-byte tag)`,
/// where the ciphertext is as long as the payload: `4 * ceil((n + 28) / 3)` bytes for `n` payload
/// bytes. That is at most `4 * (n + 30) / 3 = 4n/3 + 40`: the 28 bytes of nonce and tag plus up to
/// 2 bytes of base64 padding, each stored as 4/3 of a byte.
const SEAL_OVERHEAD_BYTES: u128 = 40;

/// The most stored bytes that `rows` sealed payloads can take when the payloads themselves total
/// at most `payload_bytes`, saturating at `i64::MAX`.
///
/// A listing bounds what it fetches by this: summing stored sizes in SQL is cheap (no row has to
/// be decrypted, nor even read from TOAST), and a run of rows whose stored size passes this bound
/// cannot fit `payload_bytes` of payload.
pub(super) fn sealed_bytes_bound(payload_bytes: usize, rows: i64) -> i64 {
    // Sum of 4 * (n + 30) / 3 over the rows, rounded up. In u128 nothing here can overflow: a
    // usize or an i64 times a few fits with room to spare.
    let payload = u128::try_from(payload_bytes).unwrap_or(u128::from(u64::MAX));
    let rows = u128::try_from(rows).unwrap_or(0);
    let sealed = (payload * 4).div_ceil(3) + SEAL_OVERHEAD_BYTES * rows;
    i64::try_from(sealed).unwrap_or(i64::MAX)
}

/// Seals and opens one agent's stored payloads (AES-256-GCM).
///
/// The key is the agent's state key (`SecretsCrypto::try_for_agent_state`), derived from the
/// platform master key apart from every other key, the agent's own secrets key included, so a
/// stolen row of one agent's state is not readable with any other key. Each ciphertext is also
/// bound to the row it was written for (`associated_data`): one copied onto another row, of this
/// agent or another, fails to open instead of answering for that row. Build one per request and
/// reuse it across the rows of that request.
pub(super) struct PayloadCipher {
    crypto: SecretsCrypto,
    agent_id: Uuid,
}

impl PayloadCipher {
    pub(super) fn for_agent(agent_id: Uuid) -> Result<Self, StateError> {
        let crypto = SecretsCrypto::try_for_agent_state(agent_id)
            .map_err(|e| StateError::Internal(format!("agent state key unavailable: {e}")))?;
        Ok(Self { crypto, agent_id })
    }

    pub(super) fn seal(&self, store: Store, name: &str, payload: &BoundedPayload<'_>) -> String {
        self.crypto
            .encrypt_with_aad(payload.0, &associated_data(self.agent_id, store, name))
    }

    /// The stored JSON text, as it was received. It is checked to still be JSON, so a damaged
    /// row is an error here rather than malformed output.
    pub(super) fn open(
        &self,
        store: Store,
        name: &str,
        sealed: &str,
    ) -> Result<Box<RawValue>, StateError> {
        let row = || format!("agent {} {} {name:?}", self.agent_id, store.label());
        let text = self
            .crypto
            .decrypt_with_aad(sealed, &associated_data(self.agent_id, store, name))
            .map_err(|e| {
                StateError::Internal(format!("{}: payload did not decrypt: {e}", row()))
            })?;
        RawValue::from_string(text)
            .map_err(|e| StateError::Internal(format!("{}: payload is not JSON: {e}", row())))
    }
}

/// The row a ciphertext belongs to, as the bytes it is sealed with: the agent id (16 bytes), the
/// store's label behind its length, then the name. Fixed and length-prefixed fields leave the
/// name as the unambiguous remainder, so no two rows share these bytes. Part of the stored
/// format: changing it makes every stored row unreadable.
///
/// The row's version is not bound, so someone able to write the database could put back an older
/// ciphertext of the same row and it would open. Database write access is outside the threat model.
fn associated_data(agent_id: Uuid, store: Store, name: &str) -> Vec<u8> {
    let label = store.label().as_bytes();
    let label_len = u8::try_from(label.len()).expect("store labels are a few bytes");
    [
        agent_id.as_bytes().as_slice(),
        &[label_len],
        label,
        name.as_bytes(),
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_string()).unwrap()
    }

    #[test]
    fn a_value_at_the_cap_is_accepted_and_one_byte_over_is_not() {
        // A JSON string of n characters is n + 2 bytes (the quotes).
        let at_cap = raw(&format!("\"{}\"", "x".repeat(98)));
        assert!(BoundedPayload::new(&at_cap, 100).is_ok());
        let over_cap = raw(&format!("\"{}\"", "x".repeat(99)));
        assert!(matches!(
            BoundedPayload::new(&over_cap, 100),
            Err(StateError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn the_cap_counts_the_bytes_as_sent_whitespace_and_escapes_included() {
        let padded = raw("{ \"a\" : 1 }");
        assert!(BoundedPayload::new(&padded, 11).is_ok());
        assert!(BoundedPayload::new(&padded, 10).is_err());

        // `\u00e9` is six bytes on the wire for a character that is two bytes of UTF-8.
        let escaped = raw("\"\\u00e9\"");
        assert!(BoundedPayload::new(&escaped, 8).is_ok());
        assert!(BoundedPayload::new(&escaped, 7).is_err());
    }

    #[test]
    fn every_part_of_a_rows_identity_changes_its_associated_data() {
        let agent = Uuid::from_u128(1);
        let base = associated_data(agent, Store::Tasks, "t1");
        assert_ne!(
            base,
            associated_data(Uuid::from_u128(2), Store::Tasks, "t1")
        );
        assert_ne!(base, associated_data(agent, Store::Kv, "t1"));
        assert_ne!(base, associated_data(agent, Store::Tasks, "t2"));
        // The length prefix keeps a label and a name from running into each other.
        assert_ne!(
            associated_data(agent, Store::Kv, "task"),
            associated_data(agent, Store::Tasks, "")
        );
    }

    #[test]
    fn the_associated_data_layout_is_fixed() {
        let bytes = associated_data(Uuid::from_u128(0x0102), Store::Kv, "k/1");
        let mut expected = Uuid::from_u128(0x0102).as_bytes().to_vec();
        expected.extend_from_slice(b"\x02kvk/1");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn a_sealed_payload_never_outgrows_the_bound() {
        let crypto = SecretsCrypto::for_backup_canary(&base64_key()).unwrap();
        for size in [0, 1, 2, 3, 4, 100, 1000, 4096, 65_537] {
            let payload = "x".repeat(size);
            let sealed = crypto.encrypt_with_aad(&payload, b"row");
            let sealed_len = i64::try_from(sealed.len()).unwrap();
            assert!(
                sealed_len <= sealed_bytes_bound(size, 1),
                "{size} payload bytes sealed into {sealed_len}"
            );
            // Tight: the bound is never more than a padding's worth above the real size.
            assert!(sealed_bytes_bound(size, 1) - sealed_len <= 4, "{size}");
        }
    }

    #[test]
    fn the_bound_grows_by_the_overhead_per_row_and_saturates() {
        assert_eq!(sealed_bytes_bound(3, 0), 4);
        assert_eq!(sealed_bytes_bound(3, 2), 4 + 2 * 40);
        assert_eq!(
            sealed_bytes_bound(3, -1),
            4,
            "a negative row count adds nothing"
        );
        assert_eq!(sealed_bytes_bound(usize::MAX, 101), i64::MAX);
    }

    /// A fixed key for a cipher built without this process's environment. The constructor that
    /// takes one is the backup canary's; its key derivation differs, its sealed format does not.
    fn base64_key() -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    }

    #[test]
    fn the_payload_is_the_text_as_received() {
        let text = "{\"z\":1, \"a\":123456789012345678901234567890,\"e\":\"\\u00e9\"}";
        let value = raw(text);
        assert_eq!(BoundedPayload::new(&value, 1024).unwrap().0, text);
    }
}
