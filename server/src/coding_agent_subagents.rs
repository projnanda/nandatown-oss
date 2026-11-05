//! The subagent link a coding-agent turn carries (`session.parent`), stored on
//! the child's `chat_sessions` row.
//!
//! A child's turns are ordinary turns — receipts, `trace_usage` rows and spend
//! all belong to the child, under the same harness agent as its parent — so
//! the link only records where the child sits. It is written at ingest and
//! read by `observability::subagents_sql`.

use nasiko_types::coding_agent::CodingAgentSessionParent;
use serde_json::Value;
use sqlx::{Postgres, Transaction};

/// Records `parent` on the child session `child_session_id`, whose parent's
/// scoped id is `parent_session_id`.
///
/// First non-null wins, per column: a later turn can fill a detail an earlier
/// one lacked (the tool call id often arrives only with the spawn's result),
/// but never repoint a child or overwrite what it already recorded — and only
/// a turn naming the same parent may fill anything. No foreign key: the parent
/// may arrive later, and the link resolves when it does. A turn adding nothing
/// writes nothing, so the row is not rewritten on every turn.
pub(crate) async fn link_parent(
    tx: &mut Transaction<'_, Postgres>,
    child_session_id: &str,
    parent_session_id: &str,
    parent: &CodingAgentSessionParent,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE chat_sessions
              SET parent_session_id = COALESCE(parent_session_id, $2),
                  spawn_tool_call_id = COALESCE(spawn_tool_call_id, $3),
                  subagent_type = COALESCE(subagent_type, $4)
            WHERE session_id = $1
              AND (parent_session_id IS NULL
                   OR (parent_session_id = $2
                       AND ((spawn_tool_call_id IS NULL AND $3::text IS NOT NULL)
                            OR (subagent_type IS NULL AND $4::text IS NOT NULL))))"#,
    )
    .bind(child_session_id)
    .bind(parent_session_id)
    .bind(parent.tool_call_id.as_deref())
    .bind(parent.agent_type.as_deref())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Whether a replayed receipt is the stored one with only its parent link
/// added. A client that learned to link subagents re-sends turns it reported
/// unlinked, under the same event ids; that is the same turn, so it is a
/// duplicate that may fill the link — not a conflicting payload.
pub(crate) fn same_turn_but_parent(stored: &Value, incoming: &Value) -> bool {
    let has_parent = |payload: &Value| payload.pointer("/session/parent").is_some();
    if !has_parent(incoming) || has_parent(stored) {
        return false;
    }
    without_parent(incoming) == *stored
}

fn without_parent(payload: &Value) -> Value {
    let mut copy = payload.clone();
    if let Some(session) = copy.get_mut("session").and_then(Value::as_object_mut) {
        session.remove("parent");
    }
    copy
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn receipt(parent: Option<Value>) -> Value {
        let mut session = json!({"id": "claude:child", "source_id": "child"});
        if let Some(parent) = parent {
            session["parent"] = parent;
        }
        json!({"event_id": "e", "session": session, "turn": {"id": "t"}})
    }

    #[test]
    fn a_link_added_to_an_unlinked_receipt_is_the_same_turn() {
        let stored = receipt(None);
        let incoming = receipt(Some(json!({"source_id": "parent"})));
        assert!(same_turn_but_parent(&stored, &incoming));
    }

    #[test]
    fn any_other_difference_is_still_a_conflict() {
        let stored = receipt(None);
        let mut incoming = receipt(Some(json!({"source_id": "parent"})));
        incoming["turn"]["id"] = json!("other");
        assert!(!same_turn_but_parent(&stored, &incoming));
    }

    #[test]
    fn a_stored_link_is_never_replaced_by_a_different_one() {
        let stored = receipt(Some(json!({"source_id": "first"})));
        let incoming = receipt(Some(json!({"source_id": "second"})));
        assert!(!same_turn_but_parent(&stored, &incoming));
    }

    #[test]
    fn an_unlinked_replay_of_an_unlinked_receipt_is_not_this_case() {
        let stored = receipt(None);
        let mut incoming = receipt(None);
        incoming["turn"]["id"] = json!("other");
        assert!(!same_turn_but_parent(&stored, &incoming));
    }
}
