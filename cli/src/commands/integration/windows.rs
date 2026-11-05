//! The windows an import reads past sessions in: grouped by when each session was last active, newest
//! first, so the work someone did today reaches the server before last month's.
//!
//! "Last 8 hours", then "Last 24 hours" (8 to 24 hours ago), then one window per calendar day (in this
//! computer's time zone) back to 30 days ago, labelled like "Oct 19", then "Older". Windows with no
//! session are left out, so there are at most 34.

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, Utc};

const RECENT_HOURS: i64 = 8;
const DAY_HOURS: i64 = 24;
const DAILY_DAYS: i64 = 30;
pub const RECENT_LABEL: &str = "Last 8 hours";
pub const DAY_LABEL: &str = "Last 24 hours";
pub const OLDER_LABEL: &str = "Older";

/// Which window a time falls in. Ordered newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Bucket {
    Recent,
    LastDay,
    /// A calendar day, by how many days before today it is (so a larger number is older).
    Day(i64, NaiveDate),
    Older,
}

impl Bucket {
    fn of(at: DateTime<Utc>, now: DateTime<Utc>, zone: FixedOffset) -> Self {
        let age = now - at;
        if age < Duration::hours(RECENT_HOURS) {
            return Self::Recent;
        }
        if age < Duration::hours(DAY_HOURS) {
            return Self::LastDay;
        }
        if age > Duration::days(DAILY_DAYS) {
            return Self::Older;
        }
        let day = at.with_timezone(&zone).date_naive();
        let today = now.with_timezone(&zone).date_naive();
        Self::Day((today - day).num_days(), day)
    }

    fn label(self) -> String {
        match self {
            Self::Recent => RECENT_LABEL.to_string(),
            Self::LastDay => DAY_LABEL.to_string(),
            Self::Day(_, day) => day.format("%b %-d").to_string(),
            Self::Older => OLDER_LABEL.to_string(),
        }
    }
}

/// One window and what falls in it, newest first.
#[derive(Debug, PartialEq, Eq)]
pub struct Window<T> {
    pub label: String,
    pub items: Vec<T>,
}

/// Puts `items`, each with when it was last active, into windows as of `now` in `zone`: newest window
/// first, newest item first within each, empty windows left out.
pub fn bucket<T>(
    items: Vec<(DateTime<Utc>, T)>,
    now: DateTime<Utc>,
    zone: FixedOffset,
) -> Vec<Window<T>> {
    let mut dated: Vec<(Bucket, DateTime<Utc>, T)> = items
        .into_iter()
        .map(|(at, item)| (Bucket::of(at, now, zone), at, item))
        .collect();
    // Window ascending (newest first), then time descending.
    dated.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let mut windows: Vec<(Bucket, Window<T>)> = Vec::new();
    for (bucket, _, item) in dated {
        match windows.last_mut() {
            Some((last, window)) if *last == bucket => window.items.push(item),
            _ => windows.push((
                bucket,
                Window {
                    label: bucket.label(),
                    items: vec![item],
                },
            )),
        }
    }
    windows.into_iter().map(|(_, window)| window).collect()
}

/// This computer's offset from UTC now, for day windows that match the user's calendar.
pub fn local_zone() -> FixedOffset {
    *chrono::Local::now().offset()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-10-21T15:00:00Z".parse().unwrap()
    }

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn labels<T>(windows: &[Window<T>]) -> Vec<&str> {
        windows.iter().map(|w| w.label.as_str()).collect()
    }

    fn ago(hours: i64) -> DateTime<Utc> {
        now() - Duration::hours(hours)
    }

    #[test]
    fn boundaries_fall_into_the_older_window() {
        let at = |age: Duration| Bucket::of(now() - age, now(), utc());

        assert_eq!(at(Duration::zero()), Bucket::Recent);
        // A clock running ahead of this one is still recent.
        assert_eq!(at(-Duration::hours(1)), Bucket::Recent);
        assert_eq!(
            at(Duration::hours(RECENT_HOURS) - Duration::seconds(1)),
            Bucket::Recent
        );
        assert_eq!(at(Duration::hours(RECENT_HOURS)), Bucket::LastDay);
        assert_eq!(at(Duration::hours(DAY_HOURS)).label(), "Oct 20");
        assert_eq!(at(Duration::days(DAILY_DAYS)).label(), "Sep 21");
        assert_eq!(
            at(Duration::days(DAILY_DAYS) + Duration::seconds(1)),
            Bucket::Older
        );
    }

    #[test]
    fn windows_run_newest_first_and_empty_ones_are_left_out() {
        let items = vec![
            (ago(24 * 40), "ancient"),
            (ago(1), "just now"),
            (ago(24 * 2 + 1), "two days ago"),
            (ago(3), "this morning"),
            (ago(10), "last night"),
        ];

        let windows = bucket(items, now(), utc());

        assert_eq!(
            labels(&windows),
            ["Last 8 hours", "Last 24 hours", "Oct 19", "Older"]
        );
        assert_eq!(windows[0].items, ["just now", "this morning"]);
        assert_eq!(windows[3].items, ["ancient"]);
    }

    #[test]
    fn days_follow_the_local_calendar() {
        // 02:00 UTC on Oct 19 is still Oct 18 in New York (UTC-4).
        let at: DateTime<Utc> = "2026-10-19T02:00:00Z".parse().unwrap();
        let new_york = FixedOffset::west_opt(4 * 3600).unwrap();

        assert_eq!(labels(&bucket(vec![(at, ())], now(), utc())), ["Oct 19"]);
        assert_eq!(labels(&bucket(vec![(at, ())], now(), new_york)), ["Oct 18"]);
    }

    #[test]
    fn a_session_every_hour_for_two_months_fits_in_forty_windows() {
        let items: Vec<(DateTime<Utc>, i64)> = (0..24 * 60).map(|h| (ago(h), h)).collect();

        let windows = bucket(items, now(), utc());

        assert!(windows.len() <= 40, "{} windows", windows.len());
        assert_eq!(windows.first().unwrap().label, RECENT_LABEL);
        assert_eq!(windows.last().unwrap().label, OLDER_LABEL);
        assert_eq!(
            windows.iter().map(|w| w.items.len()).sum::<usize>(),
            24 * 60
        );
        // Every item is newer than every item of the window after it.
        for pair in windows.windows(2) {
            assert!(pair[0].items.iter().max() < pair[1].items.iter().min());
        }
    }

    #[test]
    fn no_sessions_is_no_windows() {
        assert!(bucket(Vec::<(DateTime<Utc>, ())>::new(), now(), utc()).is_empty());
    }
}
