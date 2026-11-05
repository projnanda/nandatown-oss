//! Outbox drain, run-wide checks, and the Markdown report.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use sqlx::PgPool;

use super::Outcome;
use super::collector::Received;
use super::probe::{Phase, outbox_backlog, percentile};
use super::verify::Check;

/// API p95 under load passes when within this multiple of the idle baseline ...
const LATENCY_MULTIPLE: f64 = 3.0;
/// ... or under this many milliseconds outright.
const LATENCY_CEILING_MS: f64 = 500.0;
const DRAIN_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default)]
pub struct Drain {
    pub backlog_at_start: i64,
    pub remaining: i64,
    pub secs: f64,
}

impl Drain {
    pub fn drained(&self) -> bool {
        self.remaining == 0
    }

    fn rate(&self) -> f64 {
        (self.backlog_at_start - self.remaining) as f64 / self.secs.max(0.001)
    }
}

/// Polls the OTLP outbox until it is empty or `timeout` passes.
pub async fn wait_for_drain(db: &PgPool, timeout: Duration) -> Drain {
    let started = Instant::now();
    let backlog_at_start = outbox_backlog(db).await.unwrap_or(-1);
    let mut remaining = backlog_at_start;
    while remaining != 0 && started.elapsed() < timeout {
        tokio::time::sleep(DRAIN_POLL).await;
        remaining = outbox_backlog(db).await.unwrap_or(remaining);
    }
    Drain {
        backlog_at_start,
        remaining,
        secs: started.elapsed().as_secs_f64(),
    }
}

#[derive(sqlx::FromRow)]
struct Exports {
    delivered: i64,
    skipped: i64,
    skipped_delivered: i64,
    readback_pending: i64,
}

pub async fn global_checks(
    db: &PgPool,
    received: &Received,
    drain: &Drain,
) -> anyhow::Result<Vec<Check>> {
    let exports: Exports = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE otlp_trace_delivered_at IS NOT NULL) AS delivered,
                count(*) FILTER (WHERE otlp_state = 'skipped') AS skipped,
                count(*) FILTER (WHERE otlp_state = 'skipped'
                                   AND otlp_trace_delivered_at IS NOT NULL) AS skipped_delivered,
                count(*) FILTER (WHERE otlp_trace_delivered_at IS NOT NULL
                                   AND materialized_at IS NULL) AS readback_pending
           FROM coding_agent_telemetry_events",
    )
    .fetch_one(db)
    .await?;
    let reexports: u32 = received.traces.values().map(|n| n - 1).sum();
    Ok(vec![
        Check {
            name: "outbox drained",
            pass: drain.drained(),
            detail: format!(
                "{} waiting when imports finished, {} left after {:.0}s ({:.1}/s)",
                drain.backlog_at_start,
                drain.remaining,
                drain.secs,
                drain.rate()
            ),
        },
        Check {
            name: "collector received exactly the delivered (in-window) turns",
            pass: received.traces.len() as i64 == exports.delivered
                && exports.skipped_delivered == 0,
            detail: format!(
                "{} distinct traces received, {} receipts delivered, {} skipped (older than \
                 Tempo's window), {} skipped-but-exported, {reexports} re-exports, {} spans, \
                 {} log records",
                received.traces.len(),
                exports.delivered,
                exports.skipped,
                exports.skipped_delivered,
                received.spans,
                received.log_records
            ),
        },
        Check {
            name: "read-back backlog empty (spend written at ingest)",
            pass: exports.readback_pending == 0,
            detail: format!(
                "{} receipts waiting for Tempo read-back",
                exports.readback_pending
            ),
        },
    ])
}

/// Seconds from receipt to OTLP trace delivery, for live and historical turns.
pub async fn export_delays(db: &PgPool) -> anyhow::Result<(Vec<f64>, Vec<f64>)> {
    let rows: Vec<(bool, f64)> = sqlx::query_as(
        "SELECT otlp_historical,
                extract(epoch FROM otlp_trace_delivered_at - received_at)::float8
           FROM coding_agent_telemetry_events WHERE otlp_trace_delivered_at IS NOT NULL",
    )
    .fetch_all(db)
    .await?;
    let pick = |historical: bool| {
        rows.iter()
            .filter(|(h, _)| *h == historical)
            .map(|(_, s)| *s)
            .collect::<Vec<f64>>()
    };
    Ok((pick(false), pick(true)))
}

fn fmt_ms(value: Option<f64>) -> String {
    value.map_or("-".into(), |v| format!("{v:.0}"))
}

fn fmt_s(value: Option<f64>) -> String {
    value.map_or("-".into(), |v| format!("{v:.1}"))
}

pub fn render(outcome: &Outcome, delays: &(Vec<f64>, Vec<f64>)) -> (String, bool) {
    let mut out = String::new();
    let o = &outcome.options;
    let imports = &outcome.imports;
    let generated: usize = imports.iter().map(|(s, _)| s.generated).sum();
    let sum =
        |f: fn(&super::client::ImportStats) -> u64| imports.iter().map(|(s, _)| f(s)).sum::<u64>();
    let accepted = sum(|s| s.accepted);
    let wall = outcome.import_wall.as_secs_f64();
    let _ = writeln!(out, "# Coding-agent import load test\n");
    let _ = writeln!(
        out,
        "{} users × {} turns ({} live each) over {} days = {generated} turns; resend share {}; seed {}\n",
        o.users, o.turns, o.live_turns, o.days, o.resend_share, o.seed
    );

    let _ = writeln!(out, "## Throughput and server pushback\n");
    let _ = writeln!(out, "| metric | value |\n|---|---|");
    let rows = [
        ("import wall time (all users)", format!("{wall:.1} s")),
        (
            "accepted turns / s",
            format!("{:.0}", accepted as f64 / wall.max(0.001)),
        ),
        (
            "accepted / duplicate / rejected",
            format!(
                "{accepted} / {} / {}",
                sum(|s| s.duplicate),
                sum(|s| s.rejected)
            ),
        ),
        ("batch requests", sum(|s| s.requests).to_string()),
        ("429 (per-user rate)", sum(|s| s.status_429).to_string()),
        (
            "503 (server at capacity)",
            sum(|s| s.status_503).to_string(),
        ),
        ("413 (too large)", sum(|s| s.status_413).to_string()),
        (
            "batches cut by the 6 MiB byte budget",
            sum(|s| s.byte_capped).to_string(),
        ),
        (
            "timeouts / transport errors / other 5xx",
            format!(
                "{} / {} / {}",
                sum(|s| s.timeouts),
                sum(|s| s.transport_errors),
                sum(|s| s.server_errors)
            ),
        ),
        (
            "deliberate resends (all must be duplicate)",
            format!(
                "{} batches, {} turns not duplicate",
                sum(|s| s.resends),
                sum(|s| s.resend_not_duplicate)
            ),
        ),
        (
            "max server RSS",
            format!("{:.0} MiB", outcome.footprint.max_rss_kb as f64 / 1024.0),
        ),
        (
            "max server Postgres connections",
            outcome.footprint.max_server_connections.to_string(),
        ),
    ];
    for (k, v) in rows {
        let _ = writeln!(out, "| {k} | {v} |");
    }

    let times: Vec<f64> = imports
        .iter()
        .map(|(s, _)| s.elapsed.as_secs_f64())
        .collect();
    let batch_ms: Vec<f64> = imports
        .iter()
        .flat_map(|(s, _)| s.batch_ms.iter().map(|&m| f64::from(m)))
        .collect();
    let _ = writeln!(
        out,
        "\nPer-user import time: min {} s, p50 {} s, max {} s. Batch round trip (200s): p50 {} ms, p95 {} ms, p99 {} ms. Smallest batch any client fell to: {}.\n",
        fmt_s(times.iter().copied().reduce(f64::min)),
        fmt_s(percentile(&times, 50.0)),
        fmt_s(times.iter().copied().reduce(f64::max)),
        fmt_ms(percentile(&batch_ms, 50.0)),
        fmt_ms(percentile(&batch_ms, 95.0)),
        fmt_ms(percentile(&batch_ms, 99.0)),
        imports
            .iter()
            .map(|(s, _)| s.smallest_batch)
            .min()
            .unwrap_or(0)
    );

    let (latency_md, latency_ok) = latency_table(outcome);
    out.push_str(&latency_md);

    let _ = writeln!(out, "\n## Background export\n");
    let _ = writeln!(
        out,
        "Receipt → OTLP delivery: live turns p50 {} s / max {} s (n={}); historical in-window turns p50 {} s / max {} s (n={}).\n",
        fmt_s(percentile(&delays.0, 50.0)),
        fmt_s(delays.0.iter().copied().reduce(f64::max)),
        delays.0.len(),
        fmt_s(percentile(&delays.1, 50.0)),
        fmt_s(delays.1.iter().copied().reduce(f64::max)),
        delays.1.len()
    );
    if let Some((_, peak)) = outcome.footprint.backlog.iter().max_by_key(|(_, b)| *b) {
        let _ = writeln!(out, "Peak outbox backlog: {peak}.\n");
    }

    let _ = writeln!(
        out,
        "## Checks\n\n| check | users passing | first failure |\n|---|---|---|"
    );
    let mut all_pass = true;
    let names: Vec<&str> = outcome
        .checks
        .first()
        .map(|c| c.iter().map(|c| c.name).collect())
        .unwrap_or_default();
    for (i, name) in names.iter().enumerate() {
        let results: Vec<&Check> = outcome.checks.iter().filter_map(|c| c.get(i)).collect();
        let passing = results.iter().filter(|c| c.pass).count();
        let failure = results.iter().find(|c| !c.pass).map_or_else(
            || {
                results
                    .first()
                    .map_or(String::new(), |c| format!("(e.g. {})", c.detail))
            },
            |c| c.detail.clone(),
        );
        all_pass &= passing == results.len();
        let _ = writeln!(out, "| {name} | {passing}/{} | {failure} |", results.len());
    }
    for check in &outcome.global_checks {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            check.name,
            if check.pass { "pass" } else { "FAIL" },
            check.detail
        );
    }

    let lost = imports
        .iter()
        .any(|(s, h)| s.acknowledged.len() != h.total());
    let completed = imports.iter().all(|(s, _)| s.failure.is_none());
    let checks_ok = all_pass;
    let globals_ok = outcome.global_checks.iter().all(|c| c.pass);
    let _ = writeln!(out, "\n## Verdict\n");
    let verdicts = [
        ("zero lost turns", !lost && checks_ok),
        ("zero doubled turns / spend", checks_ok),
        ("every import completed", completed),
        ("API p95 under load within bound", latency_ok),
        ("background export drained and correct", globals_ok),
    ];
    for (name, ok) in verdicts {
        let _ = writeln!(out, "- {}: **{}**", name, if ok { "PASS" } else { "FAIL" });
    }
    let passed = verdicts.iter().all(|(_, ok)| *ok);
    (out, passed)
}

fn latency_table(outcome: &Outcome) -> (String, bool) {
    let mut out = String::from("\n## API latency (ms), probed once a second\n\n");
    out.push_str(
        "| endpoint | phase | n | p50 | p95 | p99 | errors |\n|---|---|---|---|---|---|---|\n",
    );
    let mut ok = true;
    let mut verdicts = Vec::new();
    for (index, name) in outcome.endpoints.iter().enumerate() {
        let mut p95 = [None; 3];
        for phase in Phase::ALL {
            let samples: Vec<&super::probe::Sample> = outcome
                .samples
                .iter()
                .filter(|s| s.endpoint == index && s.phase == phase)
                .collect();
            let ms: Vec<f64> = samples.iter().map(|s| s.ms).collect();
            let errors = samples.iter().filter(|s| !s.ok).count();
            p95[phase as usize] = percentile(&ms, 95.0);
            let _ = writeln!(
                out,
                "| {name} | {} | {} | {} | {} | {} | {errors} |",
                phase.label(),
                ms.len(),
                fmt_ms(percentile(&ms, 50.0)),
                fmt_ms(p95[phase as usize]),
                fmt_ms(percentile(&ms, 99.0))
            );
        }
        let (base, load) = (p95[0].unwrap_or(0.0), p95[1].unwrap_or(f64::INFINITY));
        let within_multiple = load <= LATENCY_MULTIPLE * base;
        let under_ceiling = load <= LATENCY_CEILING_MS;
        let held = match (within_multiple, under_ceiling) {
            (true, true) => "both bounds",
            (true, false) => "≤ 3× baseline",
            (false, true) => "≤ 500 ms",
            (false, false) => "neither bound",
        };
        ok &= within_multiple || under_ceiling;
        // The idle phase after the import holds the same data the load phase
        // ended with: load vs after isolates contention from data volume.
        let after = p95[2].map_or("-".into(), |v| format!("{v:.0}"));
        verdicts.push(format!(
            "- {name}: p95 {load:.0} ms under load vs {base:.0} ms baseline \
             ({after} ms idle with the imported data) → {held}"
        ));
    }
    out.push_str("\nBound: p95 under load ≤ 3× idle baseline OR ≤ 500 ms.\n\n");
    out.push_str(&verdicts.join("\n"));
    out.push('\n');
    (out, ok)
}
