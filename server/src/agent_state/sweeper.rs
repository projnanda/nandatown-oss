//! Reclaims the storage of agent state no one can read any more.
//!
//! Three passes, per store. An expired live row already reads as absent (the repository filters
//! on `expires_at`), so emptying it into a tombstone changes no answer; the tombstone keeps its
//! version for one more TTL so the entry's next write continues from it (`repo`). A tombstone
//! whose own expiry has passed is then forgotten. A row of a deleted agent is unreachable too:
//! the catalog's delete is a soft delete (`agents.deleted_at`), which never fires the foreign
//! key's cascade, and the agent's credential stops authenticating the moment it is set, so its
//! rows, tombstones included, are removed outright.
//!
//! Every server process runs one sweeper and they do not coordinate: each statement locks the
//! rows it removes with `SKIP LOCKED`, so a row that a peer sweeper or a writer holds is left for
//! the next pass instead of being waited on.

use std::time::Duration;

use sqlx::PgPool;
use tokio::time::MissedTickBehavior;

use super::repo::Store;

/// How often the sweeper wakes.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Rows removed per statement, so no single statement holds locks on an unbounded number of rows.
const BATCH_SIZE: i64 = 1000;

/// Rows one pass dealt with in each store.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StoreCounts {
    pub tasks: u64,
    pub kv: u64,
}

impl StoreCounts {
    fn record(&mut self, store: Store, removed: u64) {
        match store {
            Store::Tasks => self.tasks += removed,
            Store::Kv => self.kv += removed,
        }
    }
}

/// What one [`sweep`] pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Live rows whose `expires_at` had passed, emptied into tombstones.
    pub expired: StoreCounts,
    /// Tombstones whose own `expires_at` had passed, removed.
    pub forgotten: StoreCounts,
    /// Rows of agents that have been deleted, whatever their expiry, removed.
    pub orphaned: StoreCounts,
}

/// Background loop: sweep once per [`SWEEP_INTERVAL`], forever. `ttl_days` is how long a
/// tombstone made from an expired row is kept (`AGENT_STATE_TTL_DAYS`).
///
/// Spawned from `AppState::from_config_with_db` like the server's other periodic workers; it
/// never exits on its own and stops when the runtime is dropped. A failed pass is logged and
/// retried at the next tick.
pub async fn run(db: PgPool, ttl_days: i32) {
    let mut tick = tokio::time::interval(SWEEP_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first tick fires at once; nothing is worth sweeping at boot, as for the other workers.
    tick.tick().await;
    loop {
        tick.tick().await;
        match sweep(&db, ttl_days).await {
            Ok(swept) if swept == Swept::default() => {
                tracing::debug!("agent state sweep: nothing to reclaim");
            }
            Ok(swept) => tracing::info!(?swept, "agent state sweep reclaimed rows"),
            Err(e) => {
                tracing::warn!(error = %e, "agent state sweep failed, retrying next interval")
            }
        }
    }
}

/// One pass over both stores, of every row no one else is holding, a batch at a time: empties
/// expired rows into tombstones kept for `ttl_days`, forgets expired tombstones, and removes the
/// rows of deleted agents.
pub async fn sweep(db: &PgPool, ttl_days: i32) -> Result<Swept, sqlx::Error> {
    let mut swept = Swept::default();
    for store in Store::ALL {
        let expired = drain(db, &tombstone_expired_sql(store, ttl_days)).await?;
        swept.expired.record(store, expired);
        // A tombstone made just now expires one TTL from now, so this pass never forgets it.
        let forgotten = drain(db, &delete_expired_tombstones_sql(store)).await?;
        swept.forgotten.record(store, forgotten);
        let orphaned = drain(db, &delete_orphaned_sql(store)).await?;
        swept.orphaned.record(store, orphaned);
    }
    Ok(swept)
}

/// `ttl_days` is formatted in rather than bound, so every pass shares [`drain`]'s single
/// parameter; it is an integer, so it cannot carry anything but a number into the statement.
fn tombstone_expired_sql(store: Store, ttl_days: i32) -> String {
    let (table, name, emptied) = (store.table(), store.name_column(), store.emptied_columns());
    format!(
        "UPDATE {table}
         SET {emptied},
             deleted_at = now(),
             expires_at = now() + make_interval(days => {ttl_days})
         WHERE (agent_id, {name}) IN (
             SELECT agent_id, {name} FROM {table}
             WHERE deleted_at IS NULL AND expires_at <= now()
             ORDER BY expires_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )"
    )
}

fn delete_expired_tombstones_sql(store: Store) -> String {
    let (table, name) = (store.table(), store.name_column());
    format!(
        "DELETE FROM {table}
         WHERE (agent_id, {name}) IN (
             SELECT agent_id, {name} FROM {table}
             WHERE deleted_at IS NOT NULL AND expires_at <= now()
             ORDER BY expires_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )"
    )
}

fn delete_orphaned_sql(store: Store) -> String {
    let (table, name) = (store.table(), store.name_column());
    format!(
        "DELETE FROM {table}
         WHERE (agent_id, {name}) IN (
             SELECT s.agent_id, s.{name} FROM {table} s
             JOIN agents a ON a.id = s.agent_id
             WHERE a.deleted_at IS NOT NULL
             LIMIT $1
             FOR UPDATE OF s SKIP LOCKED
         )"
    )
}

/// Runs a batched statement (`$1` is the batch size) until a batch touches nothing, and returns
/// the total it touched.
async fn drain(db: &PgPool, batch_statement: &str) -> Result<u64, sqlx::Error> {
    let mut touched = 0;
    loop {
        let batch = sqlx::query(batch_statement)
            .bind(BATCH_SIZE)
            .execute(db)
            .await?
            .rows_affected();
        if batch == 0 {
            return Ok(touched);
        }
        touched += batch;
    }
}
