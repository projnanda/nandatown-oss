//! A seam for counting HITL pauses and resume outcomes without this crate knowing how.
//!
//! Every pause and every resume outcome is written by one of a few functions here (the store's
//! `create`, the `create_pending_*` upserts the MCP gateway calls, the `mcp_tool` resume
//! dispatcher) or by the server's own dispatcher, and those callers hold only a `PgPool`, so the
//! observer is process-wide rather than threaded through each of them. The host installs one at
//! startup; until then (and in hosts that never do) every notification is a no-op.

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};

use crate::types::{HitlRequest, ResumeStatus};

/// What a host wants to know about the HITL lifecycle. Called inline right after the write and
/// must not block.
pub trait HitlObserver: Send + Sync {
    /// A pause was detected and `row` was newly inserted. Not called when an idempotent re-create
    /// returned the already-pending row: no row was created.
    fn pause_recorded(&self, row: &HitlRequest);

    /// The resume of an answered request reached the terminal `outcome` at `finished_at`. `row` is
    /// the request as the dispatcher held it: `created_at` and `resolved_at` are what the
    /// durations start from.
    fn resume_finished(&self, row: &HitlRequest, outcome: ResumeStatus, finished_at: DateTime<Utc>);
}

static OBSERVER: OnceLock<Arc<dyn HitlObserver>> = OnceLock::new();

/// Installs the process-wide observer. Returns `false` when one is already installed, which is
/// left in place: a host that builds several application states in one process (tests) shares one
/// observer, and the instruments behind it are process-wide anyway.
pub fn install_observer(observer: Arc<dyn HitlObserver>) -> bool {
    OBSERVER.set(observer).is_ok()
}

/// See [`HitlObserver::pause_recorded`].
pub(crate) fn pause_recorded(row: &HitlRequest) {
    if let Some(observer) = OBSERVER.get() {
        observer.pause_recorded(row);
    }
}

/// See [`HitlObserver::resume_finished`]. Public: the server's own resume dispatcher reports its
/// outcomes through the same seam, so both dispatchers are metered by one path.
pub fn resume_finished(row: &HitlRequest, outcome: ResumeStatus, finished_at: DateTime<Utc>) {
    if let Some(observer) = OBSERVER.get() {
        observer.resume_finished(row, outcome, finished_at);
    }
}
