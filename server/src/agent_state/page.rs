//! Turning a listing's rows into one page: opened, bounded in bytes, and resumable.

use serde::Serialize;
use serde_json::value::RawValue;

use super::error::StateError;
use super::repo::Rows;

/// A stored row a listing can return. Tasks and key-value entries both are, which is what lets
/// one page builder serve both listings.
pub(super) trait ListedRow {
    /// What one item of the page looks like on the wire.
    type View: Serialize;

    /// The row's name within its store (task id or key), for the log.
    fn name(&self) -> &str;
    /// A cursor that resumes the listing just after this row.
    fn resume_after(&self) -> String;
    /// The row as an item, carrying its opened body or value.
    fn into_view(self, opened: Box<RawValue>) -> Self::View;
}

/// One page of a listing. `next_cursor` is `null` on the last page.
#[derive(Debug, Serialize)]
pub(super) struct Page<T> {
    pub(super) items: Vec<T>,
    pub(super) next_cursor: Option<String>,
}

/// Builds a page from the rows a listing fetched, in order, opening each row's body with `open`.
///
/// The page stops before the row that would take its bodies past `budget_bytes`, and its cursor
/// then resumes at that row, so a short page is normal. The first row that opens is always kept,
/// however large: a page that could hold nothing would hand back the cursor it was given and the
/// caller would never get past that row.
///
/// A row that does not decrypt is left out and logged rather than failing the whole listing; only
/// a `GET` of that row itself reports the error. It still counts as read, so the cursor moves past
/// it and a page made only of such rows does not trap the caller either.
pub(super) fn build_page<R: ListedRow>(
    rows: Rows<R>,
    budget_bytes: usize,
    open: impl Fn(&R) -> Result<Box<RawValue>, StateError>,
) -> Page<R::View> {
    let mut items = Vec::new();
    let mut spent_bytes = 0;
    let mut last_read: Option<String> = None;
    let mut stopped_short = false;
    for row in rows.rows {
        let opened = match open(&row) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(
                    name = row.name(),
                    ?error,
                    "agent state row left out of a listing"
                );
                last_read = Some(row.resume_after());
                continue;
            }
        };
        let size = opened.get().len();
        if !items.is_empty() && spent_bytes + size > budget_bytes {
            stopped_short = true;
            break;
        }
        spent_bytes += size;
        last_read = Some(row.resume_after());
        items.push(row.into_view(opened));
    }
    let more_follow = stopped_short || rows.has_more;
    Page {
        items,
        next_cursor: last_read.filter(|_| more_follow),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row whose body is `size` bytes, or one that does not open when `size` is `None`.
    struct FakeRow {
        name: &'static str,
        size: Option<usize>,
    }

    impl ListedRow for FakeRow {
        type View = &'static str;

        fn name(&self) -> &str {
            self.name
        }

        fn resume_after(&self) -> String {
            format!("after-{}", self.name)
        }

        fn into_view(self, _opened: Box<RawValue>) -> Self::View {
            self.name
        }
    }

    fn open_fake(row: &FakeRow) -> Result<Box<RawValue>, StateError> {
        let size = row
            .size
            .ok_or_else(|| StateError::Internal("does not decrypt".into()))?;
        // A JSON string of n - 2 characters is n bytes.
        Ok(RawValue::from_string(format!("\"{}\"", "x".repeat(size - 2))).unwrap())
    }

    fn page_of(rows: Vec<FakeRow>, has_more: bool, budget_bytes: usize) -> Page<&'static str> {
        build_page(Rows { rows, has_more }, budget_bytes, open_fake)
    }

    fn row(name: &'static str, size: usize) -> FakeRow {
        FakeRow {
            name,
            size: Some(size),
        }
    }

    fn broken(name: &'static str) -> FakeRow {
        FakeRow { name, size: None }
    }

    #[test]
    fn rows_within_the_budget_make_a_whole_page() {
        let page = page_of(vec![row("a", 10), row("b", 10)], false, 20);
        assert_eq!(page.items, ["a", "b"]);
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn a_page_stops_before_the_row_that_would_pass_the_budget_and_resumes_there() {
        let page = page_of(vec![row("a", 10), row("b", 10), row("c", 10)], false, 25);
        assert_eq!(page.items, ["a", "b"]);
        assert_eq!(page.next_cursor.as_deref(), Some("after-b"));
    }

    #[test]
    fn a_first_row_larger_than_the_budget_still_lists_alone() {
        let page = page_of(vec![row("huge", 100), row("b", 10)], false, 25);
        assert_eq!(page.items, ["huge"]);
        assert_eq!(page.next_cursor.as_deref(), Some("after-huge"));
    }

    #[test]
    fn a_full_page_with_more_rows_after_it_resumes_after_its_last_row() {
        let page = page_of(vec![row("a", 10), row("b", 10)], true, 100);
        assert_eq!(page.items, ["a", "b"]);
        assert_eq!(page.next_cursor.as_deref(), Some("after-b"));
    }

    #[test]
    fn a_row_that_does_not_open_is_left_out_and_read_past() {
        let page = page_of(vec![row("a", 10), broken("bad"), row("c", 10)], false, 100);
        assert_eq!(page.items, ["a", "c"]);
        assert_eq!(page.next_cursor, None);

        let page = page_of(vec![row("a", 10), broken("bad")], true, 100);
        assert_eq!(page.items, ["a"]);
        assert_eq!(page.next_cursor.as_deref(), Some("after-bad"));
    }

    #[test]
    fn a_page_of_rows_that_do_not_open_still_moves_the_cursor_on() {
        let page = page_of(vec![broken("x"), broken("y")], true, 100);
        assert!(page.items.is_empty());
        assert_eq!(page.next_cursor.as_deref(), Some("after-y"));
    }

    #[test]
    fn a_page_serializes_to_the_contract_shape() {
        let page = Page {
            items: vec![1, 2],
            next_cursor: None,
        };
        assert_eq!(
            serde_json::to_value(page).unwrap(),
            serde_json::json!({"items": [1, 2], "next_cursor": null})
        );
    }

    #[test]
    fn rows_that_do_not_open_spend_none_of_the_budget() {
        let page = page_of(vec![broken("bad"), row("a", 20), row("b", 5)], false, 25);
        assert_eq!(page.items, ["a", "b"]);
    }
}
