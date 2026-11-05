//! What the run watches while it runs: normal API latency, once a second, and
//! the server's footprint (RSS, Postgres connections, outbox backlog).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

const TICK: Duration = Duration::from_secs(1);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Outbox backlog is measured this many ticks apart.
const BACKLOG_EVERY: u32 = 5;
/// This tool's own database connections carry this name, so they are not
/// counted as the server's.
pub const APPLICATION_NAME: &str = "nasiko-loadtest";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Baseline = 0,
    Load = 1,
    After = 2,
}

impl Phase {
    pub const ALL: [Phase; 3] = [Phase::Baseline, Phase::Load, Phase::After];

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Phase::Baseline,
            1 => Phase::Load,
            _ => Phase::After,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Baseline => "baseline (idle, empty)",
            Phase::Load => "under import load",
            Phase::After => "after (idle, full)",
        }
    }
}

#[derive(Clone)]
pub struct Endpoint {
    pub name: String,
    pub url: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct Sample {
    pub phase: Phase,
    pub endpoint: usize,
    pub ms: f64,
    pub ok: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Footprint {
    pub max_rss_kb: u64,
    pub max_server_connections: i64,
    /// (seconds since start, receipts waiting for OTLP export).
    pub backlog: Vec<(f64, i64)>,
}

/// Shared switch the probes read: which phase a sample belongs to, and when to stop.
#[derive(Clone, Default)]
pub struct Control {
    phase: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
}

impl Control {
    pub fn set(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::SeqCst);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::SeqCst))
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// Requests every endpoint once per tick, one after another.
pub fn latency(
    http: reqwest::Client,
    endpoints: Vec<Endpoint>,
    control: Control,
) -> JoinHandle<Vec<Sample>> {
    tokio::spawn(async move {
        let mut samples = Vec::new();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        while !control.stopped() {
            tick.tick().await;
            for (index, endpoint) in endpoints.iter().enumerate() {
                let phase = control.phase();
                let started = Instant::now();
                let ok = http
                    .get(&endpoint.url)
                    .bearer_auth(&endpoint.token)
                    .timeout(PROBE_TIMEOUT)
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success());
                samples.push(Sample {
                    phase,
                    endpoint: index,
                    ms: started.elapsed().as_secs_f64() * 1_000.0,
                    ok,
                });
            }
        }
        samples
    })
}

/// Samples the server's RSS and connection count every tick, the outbox backlog less often.
pub fn footprint(db: PgPool, server_pid: Option<u32>, control: Control) -> JoinHandle<Footprint> {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut out = Footprint::default();
        let mut tick = tokio::time::interval(TICK);
        let mut ticks = 0_u32;
        while !control.stopped() {
            tick.tick().await;
            if let Some(pid) = server_pid {
                out.max_rss_kb = out.max_rss_kb.max(rss_kb(pid).await.unwrap_or(0));
            }
            let connections: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                  WHERE datname = current_database() AND application_name <> $1",
            )
            .bind(APPLICATION_NAME)
            .fetch_one(&db)
            .await
            .unwrap_or(0);
            out.max_server_connections = out.max_server_connections.max(connections);
            if ticks.is_multiple_of(BACKLOG_EVERY)
                && let Ok(waiting) = outbox_backlog(&db).await
            {
                out.backlog.push((started.elapsed().as_secs_f64(), waiting));
            }
            ticks += 1;
        }
        out
    })
}

/// Receipts the coding-agent outbox has yet to export.
pub async fn outbox_backlog(db: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM coding_agent_telemetry_events \
          WHERE otlp_state IN ('pending', 'failed', 'processing')",
    )
    .fetch_one(db)
    .await
}

async fn rss_kb(pid: u32) -> Option<u64> {
    let output = tokio::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// The `p`-th percentile (0-100) of `values`, nearest rank.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_nearest_rank() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 50.0), Some(50.0));
        assert_eq!(percentile(&values, 95.0), Some(95.0));
        assert_eq!(percentile(&values, 99.0), Some(99.0));
        assert_eq!(percentile(&[7.0], 99.0), Some(7.0));
        assert_eq!(percentile(&[], 50.0), None);
    }
}
