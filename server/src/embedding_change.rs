//! The one rule for changing an embedding provider or model that is already set.
//!
//! Switching provider (OpenAI → Gemini) and switching model inside one provider
//! (`text-embedding-3-small` → `text-embedding-3-large`) are the **same failure**, so
//! they get one rule: a change to an embedding provider or model that already had a value
//! is saved only when the caller confirms it once.
//!
//! The same-provider case is the dangerous one, because it looks safe — same account,
//! same key, same API, and the call succeeds. But the vectors come out at a different
//! size in a different space, so everything indexed before the change silently stops
//! matching.
//!
//! **Enforced here, not in a dialog.** There are two write surfaces (the per-user config
//! library and the organization settings) and three clients (UI, CLI, raw HTTP). A
//! confirmation that lives in the UI is a suggestion; one that lives behind the handler
//! is a rule. Both surfaces call [`needs_confirmation`] and return
//! [`confirmation_required`] on `Some`.
//!
//! This is a **write-time** guard on configuration only. The request path is untouched:
//! the router picks a model by presence, makes one call, and fails loudly.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

/// The `code` slug clients switch on. Stable once shipped (`API_CONVENTIONS` §2).
pub const CODE: &str = "embedding_change_requires_confirmation";

/// One side of a transition. `None` means "not set".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmbeddingTarget {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl EmbeddingTarget {
    pub fn new(provider: Option<String>, model: Option<String>) -> Self {
        // An empty string is not a value — the CRUD layers accept `""` from form fields
        // that were cleared, and treating it as set would make "cleared → cleared" look
        // like a change.
        let blank_to_none = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
        Self {
            provider: blank_to_none(provider),
            model: blank_to_none(model),
        }
    }

    fn is_set(&self) -> bool {
        self.provider.is_some() || self.model.is_some()
    }
}

/// A transition that needs the caller's confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingChange {
    pub from: EmbeddingTarget,
    pub to: EmbeddingTarget,
}

/// Whether moving from `current` to `incoming` needs a confirmation.
///
/// | Transition | Confirm? | Why |
/// | --- | --- | --- |
/// | not set → set | no | Nothing routed through this field before |
/// | set → a different value | **yes** | Vectors already stored become uncomparable |
/// | set → cleared | **yes** | Resolution silently drops to the next level — a different space |
/// | set → the same value | no | Idempotent re-save |
///
/// That last row is not a nicety. `PUT /api/settings` is a full-row replace, so the
/// settings page resends every column on every save; gating on *presence* rather than on
/// an actual *change* would prompt on every unrelated edit and train the user to click
/// straight through it. The CLI's read-modify-write update has the same shape.
///
/// The API-key field is deliberately absent: a different key for the same provider and
/// model is the same vector space. So are all the chat columns.
pub fn needs_confirmation(
    current: &EmbeddingTarget,
    incoming: &EmbeddingTarget,
) -> Option<EmbeddingChange> {
    if !current.is_set() || current == incoming {
        return None;
    }
    Some(EmbeddingChange {
        from: current.clone(),
        to: incoming.clone(),
    })
}

/// The `428 Precondition Required` a rejected change returns.
///
/// `428` rather than `409` because `409` already means "that name is taken" on these
/// routes, and a client has to be able to tell a retryable-with-confirmation rejection
/// apart from one that needs a different request.
///
/// `from`/`to` ride in `details` rather than at the top level: the shared error envelope
/// is `{error, code, details}`, and the UI's error parser reads exactly those three, so
/// a top-level field would be silently dropped before any page could show it.
pub fn confirmation_required(change: &EmbeddingChange) -> Response {
    (
        StatusCode::PRECONDITION_REQUIRED,
        axum::Json(json!({
            "error": describe(change),
            "code": CODE,
            "details": { "from": change.from, "to": change.to },
        })),
    )
        .into_response()
}

/// The wording. Says what actually happens, not that something "may" happen.
fn describe(change: &EmbeddingChange) -> String {
    let show = |t: &EmbeddingTarget| match (&t.provider, &t.model) {
        (Some(p), Some(m)) => format!("{p}/{m}"),
        (Some(p), None) => p.clone(),
        (None, Some(m)) => m.clone(),
        (None, None) => "none".to_string(),
    };
    format!(
        "Changing the embedding model from {} to {} cannot be undone for data you have \
         already indexed: those vectors were created by the old model, cannot be compared \
         with vectors from the new one, and searches will quietly return worse matches \
         until you re-index. Re-send with confirm_embedding_change=true to proceed.",
        show(&change.from),
        show(&change.to)
    )
}

/// Log the transition an operator confirmed, so the day someone asks why retrieval got
/// worse there is a record of when it changed and who changed it.
pub fn log_confirmed(scope: &str, subject: &str, actor: &str, change: &EmbeddingChange) {
    tracing::warn!(
        target: "nasiko::embedding_change",
        scope,
        subject,
        actor,
        from_provider = ?change.from.provider,
        from_model = ?change.from.model,
        to_provider = ?change.to.provider,
        to_model = ?change.to.model,
        "embedding provider/model changed after explicit confirmation; vectors indexed \
         with the previous model are no longer comparable and need re-indexing"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(provider: Option<&str>, model: Option<&str>) -> EmbeddingTarget {
        EmbeddingTarget::new(provider.map(str::to_string), model.map(str::to_string))
    }

    #[test]
    fn unset_to_set_needs_no_confirmation() {
        let current = target(None, None);
        let incoming = target(Some("openai"), Some("text-embedding-3-small"));
        assert!(needs_confirmation(&current, &incoming).is_none());
    }

    #[test]
    fn changing_the_model_within_one_provider_needs_confirmation() {
        // The dangerous case: same account, same key, same API, call succeeds — and every
        // vector stored before this moment stops matching.
        let current = target(Some("openai"), Some("text-embedding-3-small"));
        let incoming = target(Some("openai"), Some("text-embedding-3-large"));
        let change = needs_confirmation(&current, &incoming).expect("must confirm");
        assert_eq!(change.from.model.as_deref(), Some("text-embedding-3-small"));
        assert_eq!(change.to.model.as_deref(), Some("text-embedding-3-large"));
    }

    #[test]
    fn changing_the_provider_needs_confirmation() {
        let current = target(Some("openai"), Some("text-embedding-3-small"));
        let incoming = target(Some("gemini"), Some("text-embedding-004"));
        assert!(needs_confirmation(&current, &incoming).is_some());
    }

    #[test]
    fn clearing_a_set_value_needs_confirmation() {
        // Resolution silently drops to the next level, which is a different vector space.
        let current = target(Some("openai"), Some("text-embedding-3-small"));
        assert!(needs_confirmation(&current, &target(None, None)).is_some());
    }

    #[test]
    fn resending_the_identical_value_needs_no_confirmation() {
        // The full-row-replace case. Without this, every unrelated settings save prompts.
        let current = target(Some("openai"), Some("text-embedding-3-small"));
        let incoming = target(Some("openai"), Some("text-embedding-3-small"));
        assert!(needs_confirmation(&current, &incoming).is_none());
    }

    #[test]
    fn blanks_are_not_values() {
        // A cleared form field arrives as "" and must read as unset, or clearing an
        // already-cleared field would look like a change.
        let current = target(Some(""), Some("   "));
        assert!(needs_confirmation(&current, &target(None, None)).is_none());
    }

    #[test]
    fn the_message_names_both_sides_of_the_transition() {
        let change = needs_confirmation(
            &target(Some("openai"), Some("text-embedding-3-small")),
            &target(Some("gemini"), Some("text-embedding-004")),
        )
        .expect("must confirm");
        let message = describe(&change);
        assert!(message.contains("openai/text-embedding-3-small"));
        assert!(message.contains("gemini/text-embedding-004"));
        assert!(message.contains("re-index"));
    }

    #[test]
    fn clearing_reads_as_none_in_the_message() {
        let change = needs_confirmation(
            &target(Some("openai"), Some("text-embedding-3-small")),
            &target(None, None),
        )
        .expect("must confirm");
        assert!(describe(&change).contains("to none"));
    }
}
