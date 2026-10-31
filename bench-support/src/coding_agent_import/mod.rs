//! R14 load test: many users importing large coding-agent histories at once.
//!
//! Runs against a separately started server (see
//! `scripts/loadtest-coding-agent-import.sh`, which starts one on its own
//! database). Proves three things and measures the rest:
//!
//! - no turn is lost: every generated turn is acknowledged and stored;
//! - none is doubled: one receipt and one `trace_usage` row per turn, and every
//!   usage view totals exactly what was generated, despite deliberate resends;
//! - the server stays usable: normal API latency under import load is compared
//!   with an idle baseline.

pub mod client;
pub mod collector;
pub mod history;
pub mod probe;
pub mod report;
pub mod setup;
pub mod verify;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use history::{History, MODELS, PROVIDER, PriceBoundaries, Shape};
use probe::{Control, Endpoint, Phase};

#[derive(Debug, Clone, Parser)]
#[command(about = "Load-test concurrent coding-agent history imports against a running server")]
pub struct Options {
    /// Server under test.
    #[arg(long, default_value = "http://127.0.0.1:8095")]
    pub base_url: String,
    /// The server's database, for verification. Must name a database containing
    /// "loadtest", so a dev database is never touched by mistake.
    #[arg(long)]
    pub database_url: String,
    #[arg(long, default_value = "admin")]
    pub admin_username: String,
    #[arg(long)]
    pub admin_password: String,
    /// Concurrent importing users.
    #[arg(long, default_value_t = 20)]
    pub users: usize,
    /// Turns each user imports, live ones included.
    #[arg(long, default_value_t = 20_000)]
    pub turns: usize,
    /// Of those, turns that happen "now", while the import runs.
    #[arg(long, default_value_t = 5)]
    pub live_turns: usize,
    /// How far back the history reaches.
    #[arg(long, default_value_t = 90)]
    pub days: i64,
    /// Leading turns of one content session per user that carry ~1.2 MB each, so
    /// the client must cut batches by bytes, not only by count (0 disables).
    #[arg(long, default_value_t = 8)]
    pub bulky_turns: usize,
    /// Share of answered batches re-sent as if the answer was lost.
    #[arg(long, default_value_t = 0.02)]
    pub resend_share: f64,
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
    /// Prefix for usernames and session ids; reuse it to re-import the same history.
    #[arg(long)]
    pub run_id: Option<String>,
    /// Where the stand-in OTLP collector listens (the server's CODING_AGENT_OTLP_ENDPOINT).
    #[arg(long, default_value = "127.0.0.1:4399")]
    pub collector_addr: SocketAddr,
    /// Server process to sample RSS from.
    #[arg(long)]
    pub server_pid: Option<u32>,
    #[arg(long, default_value_t = 20)]
    pub baseline_secs: u64,
    #[arg(long, default_value_t = 20)]
    pub after_secs: u64,
    /// How long to wait for the OTLP outbox to drain after the imports finish.
    #[arg(long, default_value_t = 900)]
    pub drain_timeout_secs: u64,
    /// The server's CODING_AGENT_TEMPO_MAX_AGE_SECS.
    #[arg(long, default_value_t = 15 * 24 * 3600)]
    pub tempo_max_age_secs: i64,
    /// Also write the report here.
    #[arg(long)]
    pub report: Option<std::path::PathBuf>,
}

/// Everything the report is made from.
pub struct Outcome {
    pub options: Options,
    pub users: Vec<setup::User>,
    pub imports: Vec<(client::ImportStats, History)>,
    pub import_wall: Duration,
    pub samples: Vec<probe::Sample>,
    pub endpoints: Vec<String>,
    pub footprint: probe::Footprint,
    pub drain: report::Drain,
    pub collector: collector::Received,
    pub checks: Vec<Vec<verify::Check>>,
    pub global_checks: Vec<verify::Check>,
}

pub async fn run(options: Options) -> anyhow::Result<Outcome> {
    let db = connect(&options.database_url).await?;
    let collector = collector::Collector::start(options.collector_addr)
        .await
        .with_context(|| format!("binding the OTLP collector on {}", options.collector_addr))?;
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(256)
        .build()?;
    let base = options.base_url.trim_end_matches('/').to_owned();
    setup::wait_for_server(&http, &base).await?;

    let run_id = options
        .run_id
        .clone()
        .unwrap_or_else(|| format!("{:x}", Utc::now().timestamp() % 0xffffff));
    let admin = setup::admin_token(
        &http,
        &base,
        &options.admin_username,
        &options.admin_password,
    )
    .await?;
    let mut users = Vec::with_capacity(options.users);
    for index in 0..options.users {
        users.push(setup::create_user(&http, &base, &admin, &run_id, index).await?);
    }
    eprintln!(
        "created and registered {} users (run {run_id})",
        users.len()
    );

    let now = Utc::now();
    let boundaries = price_boundaries(&db).await?;
    let engine = nasiko_pricing::PricingEngine::new(db.clone());
    let rates = verify::rates(&engine, &boundaries, now).await;
    let shape = Shape {
        turns: options.turns,
        live_turns: options.live_turns.min(options.turns),
        days: options.days,
        seed: options.seed,
        bulky_turns: options.bulky_turns,
        now,
    };

    let (endpoint_names, endpoints) = probe_endpoints(&base, &users[0], &admin, now, options.days);
    let control = Control::default();
    let latency = probe::latency(http.clone(), endpoints, control.clone());
    let footprint = probe::footprint(db.clone(), options.server_pid, control.clone());
    eprintln!("baseline: {}s of idle latency", options.baseline_secs);
    tokio::time::sleep(Duration::from_secs(options.baseline_secs)).await;

    control.set(Phase::Load);
    let import_started = Instant::now();
    let imports = import_all(&http, &base, &options, &users, &run_id, shape, &boundaries).await;
    let import_wall = import_started.elapsed();
    eprintln!(
        "imports finished in {:.1}s; waiting for the outbox",
        import_wall.as_secs_f64()
    );
    let oversized = client::oversized_turn(&http, &base, &users[0]).await;
    eprintln!("oversized single turn: {}", oversized.detail);
    let drain = report::wait_for_drain(&db, Duration::from_secs(options.drain_timeout_secs)).await;

    control.set(Phase::After);
    tokio::time::sleep(Duration::from_secs(options.after_secs)).await;
    control.stop();
    let samples = latency.await?;
    let footprint = footprint.await?;

    let window = (
        now - chrono::Duration::days(options.days + 2),
        now + chrono::Duration::hours(2),
    );
    let mut checks = Vec::with_capacity(users.len());
    for (user, (stats, history)) in users.iter().zip(&imports) {
        let ctx = verify::UserContext {
            db: &db,
            http: &http,
            base: &base,
            user,
            history,
            stats,
            rates: &rates,
            window,
            tempo_max_age_secs: options.tempo_max_age_secs,
        };
        checks.push(verify::user(&ctx).await?);
    }
    let received = collector.snapshot();
    let mut global_checks = report::global_checks(&db, &received, &drain).await?;
    global_checks.push(oversized);
    Ok(Outcome {
        options,
        users,
        imports,
        import_wall,
        samples,
        endpoints: endpoint_names,
        footprint,
        drain,
        collector: received,
        checks,
        global_checks,
    })
}

async fn import_all(
    http: &reqwest::Client,
    base: &str,
    options: &Options,
    users: &[setup::User],
    run_id: &str,
    shape: Shape,
    boundaries: &PriceBoundaries,
) -> Vec<(client::ImportStats, History)> {
    let tasks: Vec<_> = users
        .iter()
        .map(|user| {
            let history = History::plan(
                run_id,
                user.index,
                &user.agent_name,
                shape,
                boundaries.clone(),
            );
            let device = client::Device {
                index: user.index,
                base_url: base.to_owned(),
                token: user.token.clone(),
                resend_share: options.resend_share,
            };
            let http = http.clone();
            tokio::spawn(async move {
                let (stats, history) = client::import(&http, &device, history).await;
                eprintln!(
                    "user {:02}: {} turns in {:.1}s ({} requests, {}×429, {}×503{})",
                    device.index,
                    stats.acknowledged.len(),
                    stats.elapsed.as_secs_f64(),
                    stats.requests,
                    stats.status_429,
                    stats.status_503,
                    stats
                        .failure
                        .as_deref()
                        .map(|f| format!(", FAILED: {f}"))
                        .unwrap_or_default()
                );
                (stats, history)
            })
        })
        .collect();
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        match task.await {
            Ok(result) => out.push(result),
            Err(error) => panic!("import task panicked: {error}"),
        }
    }
    out
}

fn probe_endpoints(
    base: &str,
    user: &setup::User,
    admin: &str,
    now: DateTime<Utc>,
    days: i64,
) -> (Vec<String>, Vec<Endpoint>) {
    let fmt = |at: DateTime<Utc>| at.format("%Y-%m-%dT%H%%3A%M%%3A%SZ").to_string();
    let window = format!(
        "start_time={}&end_time={}",
        fmt(now - chrono::Duration::days(days + 2)),
        fmt(now + chrono::Duration::hours(2))
    );
    let list = [
        ("GET /api/me (user)", "/api/me".to_owned(), &user.token),
        (
            "GET session/list (user, 90d)",
            format!("/api/observability/session/list?{window}&limit=25"),
            &user.token,
        ),
        (
            "GET finops/dashboard (user, 90d)",
            format!("/api/observability/finops/dashboard?{window}"),
            &user.token,
        ),
        (
            "GET finops/dashboard (admin, all users, 90d)",
            format!("/api/observability/finops/dashboard?{window}"),
            &admin.to_owned(),
        ),
    ];
    let names = list.iter().map(|(n, _, _)| (*n).to_owned()).collect();
    let endpoints = list
        .into_iter()
        .map(|(name, path, token)| Endpoint {
            name: name.to_owned(),
            url: format!("{base}{path}"),
            token: token.clone(),
        })
        .collect();
    (names, endpoints)
}

/// When each model's first price-book row took effect (`None`: no row at all).
async fn price_boundaries(db: &PgPool) -> anyhow::Result<PriceBoundaries> {
    let mut out = PriceBoundaries::new();
    for model in MODELS {
        let at: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT min(effective_from) FROM model_pricing \
              WHERE lower(provider) = $1 AND lower(model) = $2",
        )
        .bind(PROVIDER)
        .bind(model)
        .fetch_one(db)
        .await?;
        out.insert(model.to_owned(), at);
    }
    Ok(out)
}

async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let options: PgConnectOptions = url.parse().context("parsing --database-url")?;
    let name = options.get_database().unwrap_or_default().to_owned();
    if !name.contains("loadtest") {
        bail!("refusing to run against database '{name}': its name must contain 'loadtest'");
    }
    let options = options.application_name(probe::APPLICATION_NAME);
    Ok(PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?)
}
