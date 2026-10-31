//! Per-agent request metrics (`oss/server/src/telemetry/agent_request.rs`) on the real request
//! paths: every hop that sends a request to an agent must count it as in flight for exactly the
//! request's span, and report how it ended.
//!
//! The in-flight count is read in-process (`TestServer::genai_metrics`); outcomes are read from a
//! manual metric reader installed as the global meter provider before any server starts, which
//! is where `GenAiMetrics::new` takes its meter from.
//!
//! Requires infra (Postgres, Redis, S3) like the rest of the suite:
//!   cargo test -p nasiko-server --test agent_request_metrics -- --test-threads=1

mod common;

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::{
    InstrumentKind, ManualReader, MetricResult, Pipeline, SdkMeterProvider, Temporality,
    data::{ResourceMetrics, Sum},
    reader::MetricReader,
};
use serde_json::{Value, json};
use serial_test::serial;
use tokio::sync::{Semaphore, mpsc};
use uuid::Uuid;

// ─── Outcome reader ──────────────────────────────────────────────────────────

/// Shares one `ManualReader` between the global provider and the tests that collect from it.
#[derive(Debug, Clone)]
struct SharedReader(Arc<ManualReader>);

impl MetricReader for SharedReader {
    fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
        self.0.register_pipeline(pipeline)
    }
    fn collect(&self, rm: &mut ResourceMetrics) -> MetricResult<()> {
        self.0.collect(rm)
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn shutdown(&self) -> OTelSdkResult {
        self.0.shutdown()
    }
    fn temporality(&self, kind: InstrumentKind) -> Temporality {
        self.0.temporality(kind)
    }
}

/// Installed once per test binary, before the first `TestServer` builds its metrics.
fn metric_reader() -> &'static SharedReader {
    static READER: OnceLock<SharedReader> = OnceLock::new();
    READER.get_or_init(|| {
        let reader = SharedReader(Arc::new(ManualReader::builder().build()));
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        opentelemetry::global::set_meter_provider(provider);
        reader
    })
}

async fn start_server() -> common::TestServer {
    metric_reader();
    common::TestServer::start().await
}

/// Cumulative `nasiko.agent.requests` for `agent_id` with the given outcome.
fn requests_with_outcome(agent_id: Uuid, outcome: &str) -> u64 {
    let mut rm = ResourceMetrics {
        resource: opentelemetry_sdk::Resource::builder_empty().build(),
        scope_metrics: Vec::new(),
    };
    metric_reader().collect(&mut rm).expect("collect metrics");
    let agent = agent_id.to_string();
    rm.scope_metrics
        .iter()
        .flat_map(|scope| scope.metrics.iter())
        .filter(|metric| metric.name == "nasiko.agent.requests")
        .filter_map(|metric| metric.data.as_any().downcast_ref::<Sum<u64>>())
        .flat_map(|sum| sum.data_points.iter())
        .filter(|point| {
            let has = |key: &str, value: &str| {
                point
                    .attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
            };
            has("nasiko.agent.id", &agent) && has("outcome", outcome)
        })
        .map(|point| point.value)
        .sum()
}

fn inflight(server: &common::TestServer, agent_id: Uuid) -> u64 {
    server
        .genai_metrics
        .agent_inflight_now(&agent_id.to_string())
}

/// Polls until the agent has nothing in flight; panics with the last count on timeout.
async fn wait_for_no_inflight(server: &common::TestServer, agent_id: Uuid) {
    for _ in 0..100 {
        if inflight(server, agent_id) == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "agent {agent_id} still has {} request(s) in flight",
        inflight(server, agent_id)
    );
}

// ─── Fixtures ────────────────────────────────────────────────────────────────

async fn seed_user(server: &common::TestServer, user_id: Uuid) {
    sqlx::query(
        "INSERT INTO users (id, username, email) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .bind(format!("user_{}", &user_id.to_string()[..8]))
    .bind(format!("user_{}@test.example", &user_id.to_string()[..8]))
    .execute(&server.db)
    .await
    .expect("seed_user");
}

async fn seed_running_agent(
    server: &common::TestServer,
    owner_id: Uuid,
    name: &str,
    url: &str,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, image, status, url, is_public) \
         VALUES ($1, $2, 'x:1.0.0', 'running', $3, false) RETURNING id",
    )
    .bind(name)
    .bind(owner_id)
    .bind(url)
    .fetch_one(&server.db)
    .await
    .expect("seed_running_agent")
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

fn completed_task_reply() -> Value {
    json!({
        "jsonrpc": "2.0", "id": "1",
        "result": {"task": {
            "id": "agent-task", "contextId": "agent-ctx",
            "status": {"state": "TASK_STATE_COMPLETED"},
            "artifacts": [{"parts": [{"text": "done"}]}]
        }}
    })
}

/// An agent that announces each request on `arrived` and answers only once the test adds a
/// permit to `release`, so the test can read the in-flight count while the request is open.
struct HeldAgent {
    url: String,
    arrived: mpsc::UnboundedReceiver<()>,
    release: Arc<Semaphore>,
}

impl HeldAgent {
    async fn start() -> Self {
        let (tx, arrived) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let gate = release.clone();
        let app = Router::new().fallback(move |_req: Request| {
            let tx = tx.clone();
            let gate = gate.clone();
            async move {
                let _ = tx.send(());
                gate.acquire().await.expect("gate open").forget();
                axum::Json(completed_task_reply())
            }
        });
        Self {
            url: serve(app).await,
            arrived,
            release,
        }
    }

    async fn wait_for_request(&mut self) {
        tokio::time::timeout(Duration::from_secs(20), self.arrived.recv())
            .await
            .expect("the agent was never called")
            .expect("agent channel open");
    }
}

/// An agent that answers every request with the same JSON body, immediately.
async fn start_json_agent(reply: Value) -> String {
    let app = Router::new().fallback(move |_req: Request| {
        let reply = reply.clone();
        async move { axum::Json(reply) }
    });
    serve(app).await
}

/// An agent whose SSE reply never ends: one working event, then one every 100 ms.
async fn start_endless_sse_agent() -> String {
    let app = Router::new().fallback(|_req: Request| async {
        let event = "data: {\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{\"kind\":\"status-update\",\
                     \"taskId\":\"t\",\"contextId\":\"c\",\"status\":{\"state\":\"working\"},\
                     \"final\":false}}\n\n";
        let frames = futures::stream::unfold(true, move |first| async move {
            if !first {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Some((Ok::<_, std::io::Error>(bytes::Bytes::from(event)), false))
        });
        let mut response: Response = Body::from_stream(frames).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        response
    });
    serve(app).await
}

/// An OpenAI-compatible model: the first (streaming) turn calls `tool_name`, any later
/// (non-streaming) turn answers in text.
async fn start_tool_calling_llm(tool_name: String) -> String {
    let app = Router::new().fallback(move |req: Request| {
        let tool_name = tool_name.clone();
        async move {
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            if body["stream"] == json!(true) {
                let arguments = json!({ "message": "do the thing" }).to_string();
                let chunk = json!({"id": "cmpl-1", "model": "test", "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": tool_name, "arguments": arguments}
                    }]},
                    "finish_reason": "tool_calls"
                }]});
                return format!("data: {chunk}\n\ndata: [DONE]\n\n").into_response();
            }
            axum::Json(json!({
                "id": "cmpl-1", "object": "chat.completion", "created": 0, "model": "test",
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "All done."}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))
            .into_response()
        }
    });
    serve(app).await
}

/// SAFETY: tests in this crate run serially (`#[serial]`), as `common::test_config` assumes.
unsafe fn set_openai_env(base_url: &str) {
    unsafe {
        std::env::set_var("OPENAI_BASE_URL", base_url);
        std::env::set_var("OPENAI_API_KEY", "test-key-for-agent-request-metrics");
    }
}

fn a2a_request(metadata: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "message/stream",
        "id": Uuid::new_v4().to_string(),
        "params": {
            "message": {
                "messageId": Uuid::new_v4().to_string(),
                "contextId": format!("ctx-{}", Uuid::new_v4()),
                "role": "ROLE_USER",
                "parts": [{ "text": "please help" }]
            },
            "metadata": metadata
        }
    })
}

fn post_as(
    server: &common::TestServer,
    user_id: Uuid,
    path: &str,
    body: &Value,
) -> reqwest::RequestBuilder {
    common::as_superuser(
        server.client.post(server.url(path)).json(body),
        &user_id.to_string(),
        "metrics-tester",
    )
}

// ─── Orchestrator and HITL hops ──────────────────────────────────────────────

/// The ReAct orchestrator (`/api/orchestrator/a2a` without an `agent_id`) calls its agent
/// directly, not through the proxy: the call guard's handle must hold the count for the call.
#[tokio::test]
#[serial]
async fn an_orchestrator_routed_turn_counts_the_target_agent_in_flight() {
    let server = start_server().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let mut agent = HeldAgent::start().await;
    let agent_id = seed_running_agent(&server, user_id, "metered-target", &agent.url).await;
    let llm_url = start_tool_calling_llm("call_agent_metered_target".into()).await;
    unsafe { set_openai_env(&llm_url) };

    let turn = post_as(
        &server,
        user_id,
        "/api/orchestrator/a2a",
        &a2a_request(json!({ "agent_id": "orchestrator" })),
    )
    .send();
    let turn = tokio::spawn(async move { turn.await.unwrap().text().await.unwrap() });

    agent.wait_for_request().await;
    assert_eq!(inflight(&server, agent_id), 1, "held call is in flight");

    agent.release.add_permits(1);
    let body = turn.await.unwrap();
    wait_for_no_inflight(&server, agent_id).await;
    assert_eq!(
        requests_with_outcome(agent_id, "ok"),
        1,
        "the completed call is counted ok; response was: {body}"
    );

    server.cleanup().await;
}

/// The HITL dispatcher delivers the human's answer straight to the agent.
#[tokio::test]
#[serial]
async fn a_hitl_resume_delivery_counts_the_agent_in_flight() {
    let server = start_server().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let mut agent = HeldAgent::start().await;
    let agent_id = seed_running_agent(&server, user_id, "metered-resume", &agent.url).await;
    let hitl_id: Uuid = sqlx::query_scalar(
        "INSERT INTO hitl_requests
            (kind, origin, agent_id, owner_user_id, task_id, context_id, question, expires_at)
         VALUES ('input_required', 'direct_chat', $1, $2, $3, $4, $5, now() + interval '1 day')
         RETURNING id",
    )
    .bind(agent_id)
    .bind(user_id)
    .bind(format!("task-{}", Uuid::new_v4()))
    .bind(format!("ctx-{}", Uuid::new_v4()))
    .bind(json!({"message": "what should I do?"}))
    .fetch_one(&server.db)
    .await
    .expect("seed hitl request");

    let resolved = common::as_member(
        server
            .client
            .post(server.url(&format!("/api/hitl/{hitl_id}/resolve")))
            .json(&json!({"answer": "go ahead"})),
        &user_id.to_string(),
        "metrics-tester",
    )
    .send()
    .await
    .unwrap();
    assert!(resolved.status().is_success(), "{}", resolved.status());

    agent.wait_for_request().await;
    assert_eq!(inflight(&server, agent_id), 1, "held delivery is in flight");

    agent.release.add_permits(1);
    wait_for_no_inflight(&server, agent_id).await;
    for _ in 0..100 {
        if requests_with_outcome(agent_id, "ok") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(requests_with_outcome(agent_id, "ok"), 1);

    server.cleanup().await;
}

// ─── Explicit-agent dispatch, non-streaming replies ──────────────────────────

/// Sends one explicit-agent dispatch to an agent that always answers `reply`, and returns the
/// agent's id once the request has fully completed.
async fn dispatch_to_json_agent(server: &common::TestServer, name: &str, reply: Value) -> Uuid {
    let user_id = Uuid::new_v4();
    seed_user(server, user_id).await;
    let url = start_json_agent(reply).await;
    let agent_id = seed_running_agent(server, user_id, name, &url).await;
    let body = a2a_request(json!({ "agent_id": agent_id.to_string() }));
    // The status and body are whatever the dispatch makes of the reply; only the metric matters.
    let _ = post_as(server, user_id, "/api/orchestrator/a2a", &body)
        .send()
        .await
        .unwrap()
        .text()
        .await;
    wait_for_no_inflight(server, agent_id).await;
    agent_id
}

#[tokio::test]
#[serial]
async fn a_non_streaming_json_rpc_error_reply_is_counted_as_an_error() {
    let server = start_server().await;
    let rpc_error = json!({"jsonrpc": "2.0", "id": "1",
        "error": {"code": -32603, "message": "agent blew up"}});
    let agent_id = dispatch_to_json_agent(&server, "rpc-error-agent", rpc_error).await;
    assert_eq!(requests_with_outcome(agent_id, "error"), 1);
    assert_eq!(requests_with_outcome(agent_id, "ok"), 0);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_non_streaming_failed_task_reply_is_counted_as_an_error() {
    let server = start_server().await;
    let failed = json!({"jsonrpc": "2.0", "id": "1", "result": {"task": {
        "id": "t", "contextId": "c", "status": {"state": "TASK_STATE_FAILED"}}}});
    let agent_id = dispatch_to_json_agent(&server, "failed-task-agent", failed).await;
    assert_eq!(requests_with_outcome(agent_id, "error"), 1);
    assert_eq!(requests_with_outcome(agent_id, "ok"), 0);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_non_streaming_completed_reply_is_counted_ok() {
    let server = start_server().await;
    let agent_id = dispatch_to_json_agent(&server, "completed-agent", completed_task_reply()).await;
    assert_eq!(requests_with_outcome(agent_id, "ok"), 1);
    assert_eq!(requests_with_outcome(agent_id, "error"), 0);
    server.cleanup().await;
}

// ─── Direct proxy exit paths ─────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn the_proxy_releases_the_count_when_the_agent_is_unreachable() {
    let server = start_server().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    // Nothing listens on port 1: the connect fails before any byte reaches an agent.
    let agent_id =
        seed_running_agent(&server, user_id, "unreachable-agent", "http://127.0.0.1:1").await;

    let resp = post_as(
        &server,
        user_id,
        &format!("/api/agents/{agent_id}"),
        &a2a_request(json!({})),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 502);

    assert_eq!(inflight(&server, agent_id), 0);
    assert_eq!(requests_with_outcome(agent_id, "error"), 1);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_proxy_releases_the_count_when_the_client_disconnects_mid_stream() {
    let server = start_server().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let url = start_endless_sse_agent().await;
    let agent_id = seed_running_agent(&server, user_id, "endless-sse-agent", &url).await;

    let mut resp = post_as(
        &server,
        user_id,
        &format!("/api/agents/{agent_id}"),
        &a2a_request(json!({})),
    )
    .send()
    .await
    .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let first = resp.chunk().await.unwrap();
    assert!(first.is_some(), "the relay streams the agent's first event");
    assert_eq!(
        inflight(&server, agent_id),
        1,
        "the open stream is in flight"
    );

    // Drop the response mid-stream: the client goes away while the agent keeps sending.
    drop(resp);
    wait_for_no_inflight(&server, agent_id).await;
    assert_eq!(requests_with_outcome(agent_id, "cancelled"), 1);
    server.cleanup().await;
}
