//! After the run: did every turn land exactly once, and does every view of the
//! spend agree with what was generated?

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use nasiko_pricing::{PricingEngine, PromptConvention, RawUsage};
use serde_json::Value;
use sqlx::PgPool;

use super::client::ImportStats;
use super::history::{Expected, History, PROVIDER, PriceBoundaries};
use super::setup::User;

/// Per-turn cost rounding the server may apply (6 decimal places).
const COST_ROUNDING_PER_TURN: f64 = 1e-6;
/// Pricing is per call, so a relative drift this small is float summation.
const COST_RELATIVE_TOLERANCE: f64 = 1e-6;

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
}

fn check(name: &'static str, pass: bool, detail: String) -> Check {
    Check { name, pass, detail }
}

/// USD per token, per (model, priced before the model's first DB row), per class.
pub type Rates = HashMap<(String, bool), [f64; 4]>;

/// Unit rates from the server's own pricing engine over the same price book,
/// at a time inside each pricing regime.
pub async fn rates(
    engine: &PricingEngine,
    boundaries: &PriceBoundaries,
    now: DateTime<Utc>,
) -> Rates {
    let mut out = Rates::new();
    for (model, boundary) in boundaries {
        for before_book in [false, true] {
            let at = match (before_book, boundary) {
                (true, Some(at)) => *at - Duration::days(1),
                (true, None) => continue,
                (false, _) => now,
            };
            let mut unit = [0.0; 4];
            for (class, rate) in unit.iter_mut().enumerate() {
                let mut counts = [0_u64; 4];
                counts[class] = 1_000_000;
                let raw = RawUsage {
                    input: counts[0],
                    output: counts[1],
                    cache_read: counts[2],
                    cache_creation: counts[3],
                    total: None,
                };
                let priced = engine
                    .price(Some(PROVIDER), model, raw, PromptConvention::Exclusive, at)
                    .await;
                *rate = priced.cost.total_usd / 1_000_000.0;
            }
            out.insert((model.clone(), before_book), unit);
        }
    }
    out
}

pub fn expected_cost(expected: &Expected, rates: &Rates) -> f64 {
    expected
        .usage
        .iter()
        .map(|(key, tokens)| {
            let unit = rates
                .get(key)
                .or_else(|| rates.get(&(key.0.clone(), false)))
                .copied()
                .unwrap_or_default();
            tokens
                .iter()
                .zip(unit)
                .map(|(t, r)| *t as f64 * r)
                .sum::<f64>()
        })
        .sum()
}

pub struct UserContext<'a> {
    pub db: &'a PgPool,
    pub http: &'a reqwest::Client,
    pub base: &'a str,
    pub user: &'a User,
    pub history: &'a History,
    pub stats: &'a ImportStats,
    pub rates: &'a Rates,
    pub window: (DateTime<Utc>, DateTime<Utc>),
    pub tempo_max_age_secs: i64,
}

#[derive(sqlx::FromRow)]
struct UsageRow {
    rows: i64,
    traces: i64,
    tokens: i64,
    cost: f64,
}

#[derive(sqlx::FromRow)]
struct ReceiptRow {
    receipts: i64,
    distinct_ids: i64,
    live: i64,
    wrongly_skipped: i64,
    wrongly_exported: i64,
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    sessions: i64,
    linked: i64,
    resolved: i64,
}

pub async fn user(ctx: &UserContext<'_>) -> anyhow::Result<Vec<Check>> {
    let (db, id) = (ctx.db, ctx.user.id);
    let generated = ctx.history.total() as i64;
    let expected = &ctx.history.expected;
    let ids: HashSet<String> = ctx.history.event_ids().collect();
    let mut checks = vec![check(
        "client: every turn acknowledged, none rejected",
        ctx.stats.acknowledged == ids && ctx.stats.rejected == 0 && ctx.stats.failure.is_none(),
        format!(
            "{} of {generated} acknowledged, {} rejected{}",
            ctx.stats.acknowledged.len(),
            ctx.stats.rejected,
            ctx.stats
                .failure
                .as_deref()
                .map(|f| format!(", failed: {f}"))
                .unwrap_or_default()
        ),
    )];

    let stored: Vec<String> =
        sqlx::query_scalar("SELECT event_id FROM coding_agent_telemetry_events WHERE user_id = $1")
            .bind(id)
            .fetch_all(db)
            .await?;
    let stored_set: HashSet<&String> = stored.iter().collect();
    let missing = ids.iter().filter(|i| !stored_set.contains(i)).count();
    let receipts: ReceiptRow = sqlx::query_as(
        "SELECT count(*) AS receipts, count(DISTINCT event_id) AS distinct_ids,
                count(*) FILTER (WHERE NOT otlp_historical) AS live,
                count(*) FILTER (WHERE otlp_state = 'skipped'
                    AND turn_started_at >= received_at - make_interval(secs => $2) + interval '1 minute')
                    AS wrongly_skipped,
                count(*) FILTER (WHERE otlp_state <> 'skipped'
                    AND turn_started_at < received_at - make_interval(secs => $2) - interval '1 minute')
                    AS wrongly_exported
           FROM coding_agent_telemetry_events WHERE user_id = $1",
    )
    .bind(id)
    .bind(ctx.tempo_max_age_secs as f64)
    .fetch_one(db)
    .await?;
    checks.push(check(
        "db: exactly one receipt per generated turn",
        receipts.receipts == generated && receipts.distinct_ids == generated && missing == 0,
        format!(
            "{} rows, {} distinct, {missing} missing, of {generated}",
            receipts.receipts, receipts.distinct_ids
        ),
    ));
    checks.push(check(
        "db: Tempo window — old turns skipped, recent ones exported",
        receipts.wrongly_skipped == 0 && receipts.wrongly_exported == 0,
        format!(
            "{} wrongly skipped, {} wrongly queued for export",
            receipts.wrongly_skipped, receipts.wrongly_exported
        ),
    ));
    checks.push(check(
        "db: live turns marked live (otlp_historical = false)",
        receipts.live == expected.live_event_ids.len() as i64,
        format!(
            "{} live of {}",
            receipts.live,
            expected.live_event_ids.len()
        ),
    ));

    let usage: UsageRow = sqlx::query_as(
        "SELECT count(*) AS rows, count(DISTINCT trace_id) AS traces,
                coalesce(sum(input_tokens + output_tokens + cache_read_tokens
                             + cache_creation_tokens), 0)::bigint AS tokens,
                coalesce(sum(cost_usd), 0)::float8 AS cost
           FROM trace_usage WHERE user_id = $1",
    )
    .bind(id)
    .fetch_one(db)
    .await?;
    let want_cost = expected_cost(expected, ctx.rates);
    let tolerance = COST_ROUNDING_PER_TURN * generated as f64 + COST_RELATIVE_TOLERANCE * want_cost;
    checks.push(check(
        "db: one trace_usage row per turn",
        usage.rows == generated && usage.traces == generated,
        format!(
            "{} rows, {} distinct traces, of {generated}",
            usage.rows, usage.traces
        ),
    ));
    checks.push(check(
        "db: trace_usage tokens == generated",
        usage.tokens == expected.tokens as i64,
        format!("{} vs {}", usage.tokens, expected.tokens),
    ));
    checks.push(check(
        "db: trace_usage cost == expected (same price book)",
        (usage.cost - want_cost).abs() <= tolerance,
        format!("${:.6} vs ${want_cost:.6} (±{tolerance:.6})", usage.cost),
    ));

    let all_sessions = (ctx.history.top_level_sessions() + ctx.history.child_sessions()) as i64;
    let children = ctx.history.child_sessions() as i64;
    let sessions: SessionRow = sqlx::query_as(
        "SELECT count(*) AS sessions,
                count(*) FILTER (WHERE c.parent_session_id IS NOT NULL) AS linked,
                count(*) FILTER (WHERE EXISTS (
                    SELECT 1 FROM chat_sessions p
                     WHERE p.session_id = c.parent_session_id AND p.user_id = c.user_id
                       AND p.parent_session_id IS NULL)) AS resolved
           FROM chat_sessions c WHERE c.user_id = $1 AND c.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(db)
    .await?;
    checks.push(check(
        "db: sessions and subagent links",
        sessions.sessions == all_sessions
            && sessions.linked == children
            && sessions.resolved == children,
        format!(
            "{} sessions (want {all_sessions}), {} linked / {} resolved (want {children})",
            sessions.sessions, sessions.linked, sessions.resolved
        ),
    ));

    checks.extend(api_checks(ctx, usage.cost, all_sessions).await?);
    Ok(checks)
}

async fn get(ctx: &UserContext<'_>, path: &str) -> anyhow::Result<Value> {
    get_with(ctx, path, &[]).await
}

async fn get_with(
    ctx: &UserContext<'_>,
    path: &str,
    query: &[(&str, &str)],
) -> anyhow::Result<Value> {
    let response = ctx
        .http
        .get(format!("{}{path}", ctx.base))
        .query(query)
        .bearer_auth(&ctx.user.token)
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await?;
    anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}: {body}");
    Ok(body)
}

fn window_query(window: (DateTime<Utc>, DateTime<Utc>)) -> String {
    let fmt = |at: DateTime<Utc>| at.format("%Y-%m-%dT%H%%3A%M%%3A%SZ").to_string();
    format!("start_time={}&end_time={}", fmt(window.0), fmt(window.1))
}

async fn api_checks(
    ctx: &UserContext<'_>,
    db_cost: f64,
    all_sessions: i64,
) -> anyhow::Result<Vec<Check>> {
    let expected = &ctx.history.expected;
    let generated = ctx.history.total() as i64;
    let window = window_query(ctx.window);
    let close = |a: f64, b: f64| (a - b).abs() <= 0.01_f64.max(b.abs() * 1e-9);
    let mut checks = Vec::new();

    let usage = get(
        ctx,
        &format!("/api/observability/coding-agents/usage?{window}"),
    )
    .await?;
    let totals = &usage["data"]["totals"];
    let num = |v: &Value| v.as_i64().unwrap_or(-1);
    let cost = totals["cost_usd"].as_f64().unwrap_or(-1.0);
    checks.push(check(
        "api: coding-agents usage totals",
        num(&totals["turns"]) == generated
            && num(&totals["sessions"]) == all_sessions
            && num(&totals["tokens"]) == expected.tokens as i64
            && close(cost, db_cost),
        format!(
            "turns {}/{generated}, sessions {}/{all_sessions}, tokens {}/{}, cost {cost:.4}/{db_cost:.4}",
            totals["turns"], totals["sessions"], totals["tokens"], expected.tokens
        ),
    ));

    let finops = get(
        ctx,
        &format!("/api/observability/finops/dashboard?{window}"),
    )
    .await?;
    let tokens = num(&finops["data"]["token_usage"]["total_tokens"]);
    let cost = finops["data"]["summary"]["total_cost"]
        .as_f64()
        .unwrap_or(-1.0);
    checks.push(check(
        "api: finops dashboard totals",
        tokens == expected.tokens as i64 && close(cost, db_cost),
        format!(
            "tokens {tokens}/{}, cost {cost:.4}/{db_cost:.4}",
            expected.tokens
        ),
    ));

    let (top, subagents, traces) = list_sessions(ctx, &window).await?;
    let want_top = ctx.history.top_level_sessions() as i64;
    let want_children = ctx.history.child_sessions() as i64;
    let want_traces = ctx.history.top_level_turns() as i64;
    checks.push(check(
        "api: session list — top level, subagent counts, turns",
        top == want_top && subagents == want_children && traces == want_traces,
        format!(
            "{top}/{want_top} sessions, {subagents}/{want_children} subagents, {traces}/{want_traces} turns"
        ),
    ));
    Ok(checks)
}

/// Pages the top-level session list: (sessions, sum of subagent_count, sum of num_traces).
async fn list_sessions(ctx: &UserContext<'_>, window: &str) -> anyhow::Result<(i64, i64, i64)> {
    let (mut top, mut subagents, mut traces) = (0, 0, 0);
    let mut cursor: Option<String> = None;
    loop {
        let query: Vec<(&str, &str)> = cursor.iter().map(|c| ("cursor", c.as_str())).collect();
        let page = get_with(
            ctx,
            &format!("/api/observability/session/list?{window}&limit=100"),
            &query,
        )
        .await?;
        let data = &page["data"];
        for session in data["sessions"].as_array().map_or(&[][..], Vec::as_slice) {
            top += 1;
            subagents += session["subagent_count"].as_i64().unwrap_or(0);
            traces += session["num_traces"].as_i64().unwrap_or(0);
        }
        let next = data["pagination"]["end_cursor"].as_str().map(str::to_owned);
        if !data["pagination"]["has_next_page"]
            .as_bool()
            .unwrap_or(false)
            || next.is_none()
        {
            break;
        }
        cursor = next;
    }
    Ok((top, subagents, traces))
}
