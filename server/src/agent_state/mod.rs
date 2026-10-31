//! Per-agent durable state: A2A task snapshots and a small key-value store.
//!
//! An agent that keeps its task state in memory loses paused work on a restart, or when a resume
//! lands on another replica. This module gives every agent a platform-held store, reached with
//! its state-scoped credential (`NASIKO_AGENT_TOKEN`, minted by [`mint_state_token`]), for an
//! SDK-side store to write through to. Registering a task store marks the agent durable (`agents.state_mode`).
//!
//! # Routes, under `/api/agent-state`
//!
//! | Route | Purpose |
//! | --- | --- |
//! | `POST /register` | Declare capabilities (`tasks`, `kv`); `tasks` sets `state_mode = 'durable'` |
//! | `PUT/GET/DELETE /tasks/{task_id}` | One task: `{context_id, state, body}`, `body` opaque JSON |
//! | `GET /tasks` | List tasks, filtered by `context_id` and `state`, newest first |
//! | `PUT/GET/DELETE /kv/{key}` | One entry: `{value}`, opaque JSON |
//! | `GET /kv` | List entries by `prefix`, in key order |
//!
//! Listings take `limit` (1 to 100, default 50) and an opaque `cursor`, and answer
//! `{items, next_cursor}`. A key containing `/` must be percent-encoded in the path.
//!
//! # What every route guarantees
//!
//! - **Authenticated outside the session auth.** Callers are deployed agents, so the router checks
//!   the bearer credential itself (`auth`), and refuses a missing, unknown or revoked one with
//!   401. Only a state-scoped credential counts: the MCP gateway credential travels in URL paths
//!   and can be logged, so it is refused here like any unknown token.
//! - **Scoped to the caller.** The agent id comes from the credential and nowhere else, and every
//!   query filters on it. Another agent's row does not exist as far as the caller can tell: 404,
//!   never 403.
//! - **Versioned.** Each row carries a `version`, 1 when created and one higher per write. A write
//!   or delete with `If-Match: <version>` applies only if that is the current version (412 and
//!   the current version otherwise); a missing row is version 0. Without the header it is
//!   unconditional. Writers of one entry take turns (`repo`), so each sees the version the one
//!   before it committed and two writers never both claim the same version.
//! - **Expiring.** Every write sets `expires_at` to now plus `AGENT_STATE_TTL_DAYS`. An expired
//!   row reads as absent at once, and the [`sweeper`] empties it later; it removes every row of
//!   an agent that has been deleted.
//! - **Versions never restart.** A delete or an expiry leaves a tombstone that keeps the version
//!   for one more TTL, so the next write continues from it and an old `If-Match` cannot match a
//!   new row (`repo`).
//! - **Opaque.** A task body or value is stored and returned byte for byte as the agent sent it,
//!   never parsed into a JSON tree and re-encoded (`payload`).
//! - **Bounded.** A value over `AGENT_STATE_MAX_BODY_BYTES` as sent is a 413, and an agent
//!   making more than `AGENT_STATE_RATE_LIMIT_PER_MINUTE` requests a minute for each of its ready
//!   replicas (cached count, at least one, never read from the runtime per request) gets a 429, with
//!   `Retry-After` set to what is left of the window. Before any credential is looked up, one
//!   network address may make ten agents' worth of requests (`peer`).
//! - **Encrypted at rest.** Task bodies and values are stored as AES-256-GCM ciphertext under the
//!   agent's own state key, bound to the row they were written for (`payload`); the database
//!   never holds them in the clear, and a ciphertext moved to another row does not open there.

mod auth;
mod cursor;
mod error;
#[cfg(test)]
mod listing_db_tests;
mod page;
mod payload;
mod peer;
mod precondition;
mod repo;
mod request;
mod routes;
pub mod sweeper;
mod validation;
mod wire;

pub use auth::mint_state_token;
pub use routes::router;

/// Marks the agent `memory` again; called on every deploy (`agents::deploy_env`).
pub async fn reset_state_mode(db: &sqlx::PgPool, agent_id: uuid::Uuid) -> Result<(), sqlx::Error> {
    repo::mark_memory(db, agent_id).await
}

/// Puts back an agent's durable `state_mode` after a deploy that reset it failed: the
/// workload still running is the version that registered the task store.
pub async fn restore_durable_state_mode(
    db: &sqlx::PgPool,
    agent_id: uuid::Uuid,
) -> Result<(), sqlx::Error> {
    repo::mark_durable(db, agent_id).await.map(|_| ())
}

use nasiko_config::Config;

/// Warns, at startup, when agent state is configured to expire before a pending human-in-the-loop
/// request does. A task paused for a human keeps its state here; if the state goes first, the
/// human can still answer but the agent has nothing left to resume.
pub fn warn_if_state_expires_before_hitl_requests(config: &Config) {
    if expires_before(config.agent_state_ttl_days, config.hitl_request_ttl_days) {
        tracing::warn!(
            agent_state_ttl_days = config.agent_state_ttl_days,
            hitl_request_ttl_days = config.hitl_request_ttl_days,
            "AGENT_STATE_TTL_DAYS is shorter than HITL_REQUEST_TTL_DAYS: a task paused for a \
             human can lose its stored state while the request is still answerable"
        );
    }
}

fn expires_before(agent_state_ttl_days: i32, hitl_request_ttl_days: i64) -> bool {
    i64::from(agent_state_ttl_days) < hitl_request_ttl_days
}

#[cfg(test)]
mod tests {
    use super::expires_before;

    #[test]
    fn state_that_outlives_or_matches_a_hitl_request_is_fine() {
        assert!(!expires_before(7, 7));
        assert!(!expires_before(30, 7));
    }

    #[test]
    fn state_that_expires_first_is_flagged() {
        assert!(expires_before(3, 7));
        assert!(expires_before(1, 2));
    }
}
