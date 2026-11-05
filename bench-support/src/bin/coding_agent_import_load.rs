//! R14: many users importing large coding-agent histories at once.
//!
//! Run through `scripts/loadtest-coding-agent-import.sh`, which starts a server
//! on its own database and points its OTLP outbox at this tool's collector.
//! Exits non-zero when any bound fails.

use clap::Parser;
use nasiko_bench_support::coding_agent_import::{Options, report, run};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = Options::parse();
    let report_path = options.report.clone();
    let database_url = options.database_url.clone();
    let outcome = run(options).await?;
    let db = sqlx::PgPool::connect(&database_url).await?;
    let delays = report::export_delays(&db).await?;
    let (markdown, passed) = report::render(&outcome, &delays);
    println!("{markdown}");
    if let Some(path) = report_path {
        std::fs::write(&path, &markdown)?;
        eprintln!("report written to {}", path.display());
    }
    if !passed {
        std::process::exit(1);
    }
    Ok(())
}
