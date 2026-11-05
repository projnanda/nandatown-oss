//! How hard delivery leans on the server.
//!
//! - **Batch size** adapts per run ([`Pace`]): it starts at the server's limit, halves when the server
//!   says a batch is too large (`413`) or answers slowly, and doubles back after a few fast answers.
//! - **Server pacing**: a `429` or `503` with `Retry-After` is the server saying when to come back
//!   ([`server_wait`]); delivery waits exactly that long, plus a little jitter so devices told the same
//!   moment don't all return at once ([`resume_at`]).

use chrono::{DateTime, Utc};
use nasiko_types::CODING_AGENT_BATCH_MAX_EVENTS;
use std::time::Duration;

/// An answer slower than this halves the next batch.
const SLOW_RESPONSE: Duration = Duration::from_secs(5);
/// This many fast answers in a row double the batch again, up to the server's limit.
const FAST_ANSWERS_TO_GROW: u32 = 3;
/// The longest a `Retry-After` is honoured for; a server asking for more is asked again after this.
const MAX_SERVER_WAIT_SECS: i64 = 60 * 60;
/// Jitter is up to this fraction of the wait ...
const JITTER_DIVISOR: i64 = 10;
/// ... and never more than this.
const MAX_JITTER_MS: i64 = 30_000;

/// The batch size one delivery run sends at; kept in memory only, for as long as the run lasts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pace {
    batch_size: usize,
    fast_answers: u32,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            batch_size: CODING_AGENT_BATCH_MAX_EVENTS,
            fast_answers: 0,
        }
    }
}

impl Pace {
    /// How many turns the next batch may carry.
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// The server refused a batch of `sent` turns as too large: the next carries half as many.
    pub fn too_large(&mut self, sent: usize) {
        self.batch_size = (sent / 2).max(1);
        self.fast_answers = 0;
    }

    /// The server answered a batch after `elapsed`: slow halves the next batch, a run of fast answers
    /// doubles it.
    pub fn answered_in(&mut self, elapsed: Duration) {
        if elapsed > SLOW_RESPONSE {
            self.batch_size = (self.batch_size / 2).max(1);
            self.fast_answers = 0;
            return;
        }
        self.fast_answers += 1;
        if self.fast_answers >= FAST_ANSWERS_TO_GROW {
            self.batch_size = (self.batch_size * 2).min(CODING_AGENT_BATCH_MAX_EVENTS);
            self.fast_answers = 0;
        }
    }
}

/// How long a `Retry-After` header asks to wait from `now`: delay-seconds or an HTTP-date, at most an
/// hour. `None` when it is neither.
pub fn server_wait(retry_after: &str, now: DateTime<Utc>) -> Option<chrono::Duration> {
    let value = retry_after.trim();
    let wait = match value.parse::<u64>() {
        Ok(seconds) => chrono::Duration::seconds(i64::try_from(seconds).unwrap_or(i64::MAX)),
        Err(_) => {
            DateTime::parse_from_rfc2822(value)
                .ok()?
                .with_timezone(&Utc)
                - now
        }
    };
    Some(wait.clamp(
        chrono::Duration::zero(),
        chrono::Duration::seconds(MAX_SERVER_WAIT_SECS),
    ))
}

/// When to come back after the server asked to wait `wait`: then, plus jitter of up to a tenth of the
/// wait (at least a second, at most [`MAX_JITTER_MS`]).
pub fn resume_at(wait: chrono::Duration, now: DateTime<Utc>) -> DateTime<Utc> {
    now + wait + jitter(wait, uuid::Uuid::new_v4().as_u128())
}

/// A spread of `0..=bound` milliseconds for `wait`, picked by `entropy`.
fn jitter(wait: chrono::Duration, entropy: u128) -> chrono::Duration {
    let bound = (wait.num_milliseconds() / JITTER_DIVISOR).clamp(1_000, MAX_JITTER_MS);
    let spread = entropy % (u128::try_from(bound).unwrap_or(0) + 1);
    chrono::Duration::milliseconds(i64::try_from(spread).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FAST: Duration = Duration::from_millis(200);
    const SLOW: Duration = Duration::from_secs(6);

    #[test]
    fn a_too_large_batch_halves_down_to_one() {
        let mut pace = Pace::default();
        assert_eq!(pace.batch_size(), CODING_AGENT_BATCH_MAX_EVENTS);

        pace.too_large(100);
        assert_eq!(pace.batch_size(), 50);
        pace.too_large(3);
        assert_eq!(pace.batch_size(), 1);
        pace.too_large(1);
        assert_eq!(pace.batch_size(), 1);
    }

    #[test]
    fn slow_answers_halve_and_a_run_of_fast_ones_grows_back_to_the_limit() {
        let mut pace = Pace::default();
        pace.answered_in(SLOW);
        pace.answered_in(SLOW);
        assert_eq!(pace.batch_size(), 25);

        pace.answered_in(FAST);
        pace.answered_in(FAST);
        assert_eq!(pace.batch_size(), 25, "two fast answers are not yet a run");
        pace.answered_in(FAST);
        assert_eq!(pace.batch_size(), 50);
        for _ in 0..FAST_ANSWERS_TO_GROW * 3 {
            pace.answered_in(FAST);
        }
        assert_eq!(pace.batch_size(), CODING_AGENT_BATCH_MAX_EVENTS);
    }

    #[test]
    fn a_slow_answer_breaks_a_run_of_fast_ones() {
        let mut pace = Pace::default();
        pace.too_large(100);
        pace.answered_in(FAST);
        pace.answered_in(FAST);
        pace.answered_in(SLOW);
        pace.answered_in(FAST);
        pace.answered_in(FAST);

        assert_eq!(pace.batch_size(), 25);
    }

    #[test]
    fn retry_after_is_read_as_seconds_or_an_http_date_and_capped_at_an_hour() {
        let now = Utc.with_ymd_and_hms(2026, 10, 21, 8, 0, 0).unwrap();

        assert_eq!(
            server_wait("120", now),
            Some(chrono::Duration::seconds(120))
        );
        assert_eq!(server_wait(" 0 ", now), Some(chrono::Duration::zero()));
        assert_eq!(
            server_wait("Wed, 21 Oct 2026 08:01:30 GMT", now),
            Some(chrono::Duration::seconds(90))
        );
        assert_eq!(
            server_wait("Wed, 21 Oct 2026 07:00:00 GMT", now),
            Some(chrono::Duration::zero()),
            "a date already past means now"
        );
        assert_eq!(
            server_wait("86400", now),
            Some(chrono::Duration::seconds(MAX_SERVER_WAIT_SECS))
        );
        assert_eq!(server_wait("soon", now), None);
        assert_eq!(server_wait("-5", now), None);
    }

    #[test]
    fn jitter_is_a_small_bounded_spread_on_top_of_the_wait() {
        let wait = chrono::Duration::seconds(120);
        assert_eq!(jitter(wait, 0), chrono::Duration::zero());
        assert_eq!(jitter(wait, 12_000), chrono::Duration::seconds(12));
        assert_eq!(jitter(wait, 12_001), chrono::Duration::zero());
        assert!(jitter(chrono::Duration::hours(1), u128::MAX) <= chrono::Duration::seconds(30));
        assert!(jitter(chrono::Duration::zero(), u128::MAX) <= chrono::Duration::seconds(1));

        let now = Utc::now();
        let at = resume_at(wait, now);
        assert!(at >= now + wait && at <= now + wait + chrono::Duration::seconds(12));
    }
}
