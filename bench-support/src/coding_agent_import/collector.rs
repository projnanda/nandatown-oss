//! A stand-in OTLP/HTTP collector for the server's coding-agent outbox
//! (`CODING_AGENT_OTLP_ENDPOINT`). It accepts every export and counts what
//! arrived, so the run neither touches the shared Tempo nor needs it, and can
//! check that no turn older than Tempo's window was exported.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

/// The outbox sends one turn per request; this is generous.
const MAX_EXPORT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Default, Clone)]
pub struct Received {
    /// Trace exports per trace id: more than one is a re-export.
    pub traces: HashMap<String, u32>,
    pub spans: u64,
    pub log_records: u64,
}

#[derive(Clone, Default)]
pub struct Collector {
    received: Arc<Mutex<Received>>,
}

impl Collector {
    /// Serves on `addr` until the process exits.
    pub async fn start(addr: SocketAddr) -> anyhow::Result<Self> {
        let collector = Self::default();
        let app = Router::new()
            .route("/v1/traces", post(traces))
            .route("/v1/logs", post(logs))
            .layer(DefaultBodyLimit::max(MAX_EXPORT_BYTES))
            .with_state(collector.clone());
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("mock OTLP collector stopped: {error}");
            }
        });
        Ok(collector)
    }

    pub fn snapshot(&self) -> Received {
        self.received.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

async fn traces(State(collector): State<Collector>, Json(body): Json<Value>) -> Json<Value> {
    let spans: Vec<&Value> = items(&body, "resourceSpans", "scopeSpans", "spans");
    if let Ok(mut received) = collector.received.lock() {
        received.spans += spans.len() as u64;
        let ids: std::collections::HashSet<&str> = spans
            .iter()
            .filter_map(|span| span.get("traceId").and_then(Value::as_str))
            .collect();
        for id in ids {
            *received.traces.entry(id.to_owned()).or_default() += 1;
        }
    }
    Json(json!({}))
}

async fn logs(State(collector): State<Collector>, Json(body): Json<Value>) -> Json<Value> {
    let records = items(&body, "resourceLogs", "scopeLogs", "logRecords").len();
    if let Ok(mut received) = collector.received.lock() {
        received.log_records += records as u64;
    }
    Json(json!({}))
}

/// `body[outer][*][middle][*][inner][*]`, flattened.
fn items<'a>(body: &'a Value, outer: &str, middle: &str, inner: &str) -> Vec<&'a Value> {
    let list = |value: &'a Value, key: &str| -> &'a [Value] {
        value
            .get(key)
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    };
    list(body, outer)
        .iter()
        .flat_map(|resource| list(resource, middle))
        .flat_map(|scope| list(scope, inner))
        .collect()
}
