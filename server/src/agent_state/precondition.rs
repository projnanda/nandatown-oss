//! The `If-Match` precondition on a write.

use axum::extract::FromRequestParts;
use axum::http::header::IF_MATCH;
use axum::http::request::Parts;

use super::error::StateError;

/// The version a write expects the row to be at, or no expectation at all.
///
/// A row that is absent, or has expired, is at version 0, so `If-Match: 0` reads as "only if it is
/// not there yet" and a missing row fails any other expectation. With no header the write is
/// unconditional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Precondition(Option<i64>);

impl Precondition {
    pub(super) const UNCONDITIONAL: Self = Self(None);

    /// Whether a write may go ahead against a row currently at `current_version`.
    pub(super) fn holds_for(self, current_version: i64) -> bool {
        self.0.is_none_or(|expected| expected == current_version)
    }

    /// Accepts a bare integer, or one wrapped in double quotes as an entity tag would be. Anything
    /// else (signs, weak validators, lists, `*`) is refused instead of guessed at.
    fn parse(raw: &str) -> Result<Self, StateError> {
        let trimmed = raw.trim();
        let digits = trimmed
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or(trimmed);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(malformed());
        }
        digits
            .parse::<i64>()
            .map(|version| Self(Some(version)))
            .map_err(|_| malformed())
    }
}

fn malformed() -> StateError {
    StateError::invalid("If-Match must be a version number")
}

impl<S: Send + Sync> FromRequestParts<S> for Precondition {
    type Rejection = StateError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, StateError> {
        let mut values = parts.headers.get_all(IF_MATCH).iter();
        let Some(value) = values.next() else {
            return Ok(Self::UNCONDITIONAL);
        };
        if values.next().is_some() {
            return Err(StateError::invalid("If-Match may be given only once"));
        }
        Self::parse(value.to_str().map_err(|_| malformed())?)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Request};

    use super::*;

    fn parse(raw: &str) -> Option<i64> {
        match Precondition::parse(raw) {
            Ok(Precondition(version)) => version,
            Err(_) => None,
        }
    }

    #[test]
    fn a_bare_or_quoted_integer_is_a_version() {
        assert_eq!(parse("3"), Some(3));
        assert_eq!(parse("  3  "), Some(3));
        assert_eq!(parse("\"3\""), Some(3));
        assert_eq!(parse("0"), Some(0));
        assert_eq!(parse("9223372036854775807"), Some(i64::MAX));
    }

    #[test]
    fn anything_that_is_not_a_plain_integer_is_refused() {
        for bad in [
            "",
            " ",
            "abc",
            "-1",
            "+1",
            "1.5",
            "*",
            "W/\"3\"",
            "\"3",
            "3\"",
            "3, 4",
            "9223372036854775808",
            "0x10",
        ] {
            assert!(
                Precondition::parse(bad).is_err(),
                "If-Match {bad:?} must be refused"
            );
        }
    }

    #[test]
    fn an_unconditional_write_always_proceeds() {
        let none = Precondition::UNCONDITIONAL;
        assert!(none.holds_for(0));
        assert!(none.holds_for(41));
    }

    #[test]
    fn a_conditional_write_proceeds_only_at_the_expected_version() {
        let at_three = Precondition(Some(3));
        assert!(at_three.holds_for(3));
        assert!(!at_three.holds_for(2));
        assert!(!at_three.holds_for(4));
    }

    #[test]
    fn zero_means_the_row_must_be_absent() {
        let must_be_absent = Precondition(Some(0));
        assert!(must_be_absent.holds_for(0));
        assert!(!must_be_absent.holds_for(1));
        // And a missing row (version 0) fails every other expectation.
        assert!(!Precondition(Some(5)).holds_for(0));
    }

    async fn extract(headers: &[&str]) -> Result<Precondition, StateError> {
        let mut builder = Request::builder().uri("/");
        for value in headers {
            builder = builder.header(IF_MATCH, HeaderValue::from_str(value).unwrap());
        }
        let (mut parts, _) = builder.body(()).unwrap().into_parts();
        Precondition::from_request_parts(&mut parts, &()).await
    }

    #[tokio::test]
    async fn no_header_means_no_precondition() {
        assert_eq!(extract(&[]).await.unwrap(), Precondition::UNCONDITIONAL);
    }

    #[tokio::test]
    async fn the_header_is_read_from_the_request() {
        assert_eq!(extract(&["7"]).await.unwrap(), Precondition(Some(7)));
    }

    #[tokio::test]
    async fn a_repeated_header_is_refused() {
        assert!(extract(&["1", "2"]).await.is_err());
    }
}
