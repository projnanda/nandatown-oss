//! The limits and rules every name, filter and page size is held to.

use super::error::StateError;

/// Longest task id, in bytes.
pub(super) const MAX_TASK_ID_BYTES: usize = 256;
/// Longest key-value key, in bytes.
pub(super) const MAX_KEY_BYTES: usize = 512;
/// Longest context id on a task, in bytes. The contract leaves it open; this matches the task id.
pub(super) const MAX_CONTEXT_ID_BYTES: usize = 256;
/// Longest task state label, in bytes. States are short protocol words, never free text.
pub(super) const MAX_TASK_STATE_BYTES: usize = 128;
/// Longest client name an agent may report when registering, in bytes.
pub(super) const MAX_CLIENT_NAME_BYTES: usize = 128;

/// Page size when the caller does not ask for one.
pub(super) const DEFAULT_PAGE_SIZE: i64 = 50;
/// Largest page a caller may ask for.
pub(super) const MAX_PAGE_SIZE: i64 = 100;

/// Accepts a name the store can hold: non-empty, at most `max_bytes` bytes of UTF-8, and free of
/// control characters. NUL is among them and is the one that matters most: Postgres text cannot
/// contain it, so letting it through would surface as a 500 instead of a 400.
pub(super) fn check_name(field: &str, value: &str, max_bytes: usize) -> Result<(), StateError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(StateError::invalid(format!(
            "{field} must be 1 to {max_bytes} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(StateError::invalid(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

/// Reads the `limit` query value, defaulting when absent and refusing anything outside
/// `1..=MAX_PAGE_SIZE` rather than silently clamping it.
pub(super) fn page_size(raw: Option<&str>) -> Result<i64, StateError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_PAGE_SIZE);
    };
    match raw.parse::<i64>() {
        Ok(size) if (1..=MAX_PAGE_SIZE).contains(&size) => Ok(size),
        _ => Err(StateError::invalid(format!(
            "limit must be an integer from 1 to {MAX_PAGE_SIZE}"
        ))),
    }
}

/// A query value that was sent but left empty means the caller did not mean to filter on it, so
/// the filter is absent rather than a match for the empty string (which no row can hold).
pub(super) fn non_empty(raw: Option<String>) -> Option<String> {
    raw.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_rejected(result: Result<(), StateError>) -> bool {
        matches!(result, Err(StateError::BadRequest { .. }))
    }

    #[test]
    fn a_name_is_accepted_up_to_its_byte_limit() {
        assert!(check_name("key", "a", MAX_KEY_BYTES).is_ok());
        assert!(check_name("key", &"a".repeat(MAX_KEY_BYTES), MAX_KEY_BYTES).is_ok());
        assert!(is_rejected(check_name(
            "key",
            &"a".repeat(MAX_KEY_BYTES + 1),
            MAX_KEY_BYTES
        )));
    }

    #[test]
    fn a_name_is_counted_in_bytes_not_characters() {
        // 'é' is two bytes: 256 of them fit a 512-byte limit exactly, 257 do not.
        assert!(check_name("key", &"é".repeat(256), MAX_KEY_BYTES).is_ok());
        assert!(is_rejected(check_name(
            "key",
            &"é".repeat(257),
            MAX_KEY_BYTES
        )));
    }

    #[test]
    fn an_empty_name_is_rejected() {
        assert!(is_rejected(check_name("key", "", MAX_KEY_BYTES)));
    }

    #[test]
    fn control_characters_are_rejected_wherever_they_sit() {
        for bad in [
            "a\0b",
            "\nkey",
            "key\r",
            "tab\there",
            "del\u{7f}",
            "\u{85}next",
        ] {
            assert!(
                is_rejected(check_name("key", bad, MAX_KEY_BYTES)),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn printable_unicode_and_punctuation_are_accepted() {
        for good in ["app/session 1", "naïve ✓", "a?b#c%d", "日本語", "x y"] {
            assert!(
                check_name("key", good, MAX_KEY_BYTES).is_ok(),
                "{good:?} must be accepted"
            );
        }
    }

    #[test]
    fn the_error_names_the_field_it_rejected() {
        let Err(StateError::BadRequest { message, .. }) = check_name("task_id", "", 8) else {
            panic!("expected a bad request");
        };
        assert!(message.starts_with("task_id "));
    }

    #[test]
    fn page_size_defaults_when_absent() {
        assert_eq!(page_size(None).unwrap(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn page_size_accepts_exactly_one_through_the_maximum() {
        assert_eq!(page_size(Some("1")).unwrap(), 1);
        assert_eq!(page_size(Some("100")).unwrap(), MAX_PAGE_SIZE);
    }

    #[test]
    fn page_size_refuses_out_of_range_and_non_numeric_values() {
        for bad in ["0", "101", "-1", "", "abc", "1.5", "99999999999999999999"] {
            assert!(
                matches!(page_size(Some(bad)), Err(StateError::BadRequest { .. })),
                "limit={bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn an_empty_query_value_is_no_filter() {
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(Some("ctx".into())), Some("ctx".into()));
    }
}
