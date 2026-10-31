//! Opaque pagination cursors.
//!
//! A cursor is the sort position of the last row on a page, so the next page resumes after it
//! rather than at an offset, which would skew under concurrent writes. It is base64url JSON:
//! opaque to callers, but neither secret nor signed. It never carries an agent id, so a forged
//! cursor can only move within the caller's own rows.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::error::StateError;
use super::validation::{MAX_KEY_BYTES, MAX_TASK_ID_BYTES, check_name};

/// Where a page of tasks ended. Tasks sort newest `updated_at` first, then by id.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TaskCursor {
    pub(super) updated_at: DateTime<Utc>,
    pub(super) task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskCursorWire {
    updated_at_micros: i64,
    task_id: String,
}

impl TaskCursor {
    pub(super) fn encode(&self) -> String {
        // Microseconds, because that is Postgres's own timestamp resolution: a coarser cursor
        // would skip or repeat rows that share a millisecond.
        encode_json(&json!({
            "updated_at_micros": self.updated_at.timestamp_micros(),
            "task_id": self.task_id,
        }))
    }

    pub(super) fn decode(raw: &str) -> Result<Self, StateError> {
        let wire: TaskCursorWire = decode_json(raw)?;
        check_name("cursor", &wire.task_id, MAX_TASK_ID_BYTES)
            .map_err(|_| StateError::invalid_cursor())?;
        let updated_at = DateTime::from_timestamp_micros(wire.updated_at_micros)
            .ok_or_else(StateError::invalid_cursor)?;
        Ok(Self {
            updated_at,
            task_id: wire.task_id,
        })
    }
}

/// Where a page of key-value entries ended. Entries sort by key.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct KeyCursor {
    pub(super) key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyCursorWire {
    key: String,
}

impl KeyCursor {
    pub(super) fn encode(&self) -> String {
        encode_json(&json!({ "key": self.key }))
    }

    pub(super) fn decode(raw: &str) -> Result<Self, StateError> {
        let wire: KeyCursorWire = decode_json(raw)?;
        check_name("cursor", &wire.key, MAX_KEY_BYTES).map_err(|_| StateError::invalid_cursor())?;
        Ok(Self { key: wire.key })
    }
}

fn encode_json(value: &serde_json::Value) -> String {
    URL_SAFE_NO_PAD.encode(value.to_string())
}

fn decode_json<T: DeserializeOwned>(raw: &str) -> Result<T, StateError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| StateError::invalid_cursor())?;
    serde_json::from_slice(&bytes).map_err(|_| StateError::invalid_cursor())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_invalid_cursor<T: std::fmt::Debug>(result: Result<T, StateError>) -> bool {
        matches!(
            result,
            Err(StateError::BadRequest {
                code: "invalid_cursor",
                ..
            })
        )
    }

    #[test]
    fn a_task_cursor_survives_a_round_trip_to_the_microsecond() {
        let cursor = TaskCursor {
            updated_at: DateTime::from_timestamp_micros(1_790_000_000_123_456).unwrap(),
            task_id: "task-ü/1".into(),
        };
        assert_eq!(TaskCursor::decode(&cursor.encode()).unwrap(), cursor);
    }

    #[test]
    fn a_key_cursor_survives_a_round_trip() {
        let cursor = KeyCursor {
            key: "app/session 1?x=%".into(),
        };
        assert_eq!(KeyCursor::decode(&cursor.encode()).unwrap(), cursor);
    }

    #[test]
    fn an_encoded_cursor_is_url_safe() {
        let cursor = KeyCursor {
            key: "???>>>~~~".into(),
        };
        let encoded = cursor.encode();
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{encoded} would need escaping in a query string"
        );
    }

    #[test]
    fn text_that_is_not_base64_is_refused() {
        assert!(is_invalid_cursor(TaskCursor::decode("not base64!!")));
        assert!(is_invalid_cursor(KeyCursor::decode("%%%")));
    }

    #[test]
    fn base64_that_is_not_the_expected_json_is_refused() {
        let not_json = URL_SAFE_NO_PAD.encode("hello");
        assert!(is_invalid_cursor(TaskCursor::decode(&not_json)));
        let wrong_shape = URL_SAFE_NO_PAD.encode(r#"{"key":"a"}"#);
        assert!(is_invalid_cursor(TaskCursor::decode(&wrong_shape)));
    }

    #[test]
    fn a_task_cursor_is_not_a_key_cursor() {
        let task = TaskCursor {
            updated_at: DateTime::from_timestamp_micros(1).unwrap(),
            task_id: "t".into(),
        };
        assert!(is_invalid_cursor(KeyCursor::decode(&task.encode())));
    }

    #[test]
    fn unknown_cursor_fields_are_refused() {
        let extra = URL_SAFE_NO_PAD.encode(r#"{"key":"a","extra":1}"#);
        assert!(is_invalid_cursor(KeyCursor::decode(&extra)));
    }

    #[test]
    fn a_cursor_naming_an_unstorable_key_is_refused_not_passed_to_the_database() {
        // A NUL in a bind parameter would be a Postgres error, i.e. a 500 for a client mistake.
        let nul_key = URL_SAFE_NO_PAD.encode(r#"{"key":"a\u0000b"}"#);
        assert!(is_invalid_cursor(KeyCursor::decode(&nul_key)));
        let nul_task = URL_SAFE_NO_PAD.encode(r#"{"updated_at_micros":1,"task_id":"a\u0000b"}"#);
        assert!(is_invalid_cursor(TaskCursor::decode(&nul_task)));
        let empty_key = URL_SAFE_NO_PAD.encode(r#"{"key":""}"#);
        assert!(is_invalid_cursor(KeyCursor::decode(&empty_key)));
    }

    #[test]
    fn a_timestamp_outside_the_representable_range_is_refused() {
        let out_of_range = URL_SAFE_NO_PAD.encode(format!(
            r#"{{"updated_at_micros":{},"task_id":"t"}}"#,
            i64::MAX
        ));
        assert!(is_invalid_cursor(TaskCursor::decode(&out_of_range)));
    }
}
