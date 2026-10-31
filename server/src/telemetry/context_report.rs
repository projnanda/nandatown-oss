//! Recording what history selection carried, for one request.
//!
//! One measurement, two sinks, written side by side so they cannot disagree:
//!
//! * **The request span** (`a2a.dispatch` / `a2a.proxy`) carries it as `nasiko.context.*`
//!   attributes, which is the only place the trace page can read it from — that page reads Tempo,
//!   not Postgres.
//! * **`context_reports`** carries it as a row, which is the only shape the Optimization page can
//!   aggregate — windowed, agent-filtered, grouped by day.
//!
//! Counts only. Never message content: an agent's own exporter can read the spans in its trace.

use nasiko_orchestrator::{ContextReport, ReportAttribution};
use sqlx::PgPool;
use uuid::Uuid;

/// Owned copy of the attribution, so the database write can outlive the request.
pub struct ContextRecord {
    pub user_id: Uuid,
    pub agent_id: Option<Uuid>,
    pub session_id: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
}

/// Attach the report to `span` and persist it.
///
/// The write is spawned: a chat turn must not wait on accounting, and a failed write must not
/// surface on a request that already succeeded. The span attributes are set inline, because the
/// span is closed as soon as the turn ends.
///
/// A request whose pool was empty records nothing at all — a brand-new chat has no history to have
/// optimized, and a row of zeros would dilute every average the page computes.
pub fn record(db: &PgPool, span: &tracing::Span, report: &ContextReport, record: ContextRecord) {
    if report.pool_messages == 0 {
        return;
    }

    set_span_attributes(span, report);

    let db = db.clone();
    let report = report.clone();
    tokio::spawn(async move {
        nasiko_orchestrator::record_context_report(
            &db,
            &ReportAttribution {
                user_id: record.user_id,
                agent_id: record.agent_id,
                session_id: record.session_id.as_deref(),
                trace_id: record.trace_id.as_deref(),
                span_id: record.span_id.as_deref(),
            },
            &report,
        )
        .await;
    });
}

/// The `nasiko.context.*` attribute set.
///
/// `set_attribute` rather than pre-declared `tracing::field::Empty` fields: half of these are
/// optional, and an unset declared field still ships as an empty attribute that a consumer has to
/// tell apart from a real zero.
fn set_span_attributes(span: &tracing::Span, report: &ContextReport) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    // The four the trace block requires. Missing any one of them makes it render nothing, so they
    // are set unconditionally.
    span.set_attribute("nasiko.context.pool", report.pool_messages as i64);
    span.set_attribute("nasiko.context.pool_tokens", report.pool_tokens as i64);
    span.set_attribute("nasiko.context.kept", report.kept_messages as i64);
    span.set_attribute("nasiko.context.kept_tokens_est", report.kept_tokens as i64);

    if let Some(strategy) = report.strategy {
        span.set_attribute("nasiko.context.strategy", strategy);
    }
    if let Some(level) = report.level {
        span.set_attribute("nasiko.context.level", level);
    }
    if let Some(budget) = report.budget {
        span.set_attribute("nasiko.context.budget", budget as i64);
    }
    // Omitted rather than sent as 0 when compression did not run: "it ran and removed nothing" and
    // "it never ran" are different facts, and the attribute is defined as the former.
    if report.compressed_bytes_saved > 0 {
        span.set_attribute(
            "nasiko.context.compressed_bytes_saved",
            report.compressed_bytes_saved as i64,
        );
    }
    if report.fallback {
        span.set_attribute("nasiko.context.fallback", true);
    }
}
