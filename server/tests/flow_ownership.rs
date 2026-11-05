//! A request cannot open, re-open or join a flow that belongs to another user.
//!
//! A flow id is a trace id, and the trace id of an inbound request comes from the caller's own
//! `traceparent` header. Everything the platform later resolves from a flow — whose memory, whose
//! MCP permissions — resolves to the flow's `user_id`, so a caller who replays someone else's
//! traceparent must be refused, not adopted into that flow.
//!
//! Each case is tested twice: replaying a foreign flow is refused and leaves the flow and the
//! target agent untouched, and replaying one's own flow still works, so the refusal cannot pass
//! by blocking every request that carries a traceparent.
//!
//! The orchestrator and direct-dispatch handlers (`a2a_dispatch.rs`) use the same two helpers, but
//! they take their trace id from the request's OpenTelemetry span, which only inherits the
//! caller's `traceparent` when an OTel layer is installed. The test server installs none, so those
//! handlers can only be covered here through the helpers themselves.

mod common;

use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

async fn init_admin(server: &common::TestServer) -> Uuid {
    let v = server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    Uuid::parse_str(v["user_id"].as_str().unwrap()).unwrap()
}

async fn seed_member(server: &common::TestServer, username: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, is_superuser) VALUES ($1, $2, false) RETURNING id",
    )
    .bind(username)
    .bind(format!("{username}@test.local"))
    .fetch_one(&server.db)
    .await
    .unwrap()
}

/// A public agent, so any member passes the user-to-agent check and the flow ownership check is
/// the only thing standing between the caller and the flow.
async fn seed_public_agent(
    server: &common::TestServer,
    owner: Uuid,
    name: &str,
    url: &str,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, image, status, url, is_public) \
         VALUES ($1, $2, 'x:1.0.0', 'running', $3, true) RETURNING id",
    )
    .bind(name)
    .bind(owner)
    .bind(url)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

/// Stub agent answering any POST with a flat A2A message; counts how many requests reached it.
async fn start_stub_agent() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let calls: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
    let counted = calls.clone();
    let a2a = post(move || {
        let counted = counted.clone();
        async move {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Json(json!({
                "jsonrpc": "2.0", "id": "1",
                "result": {"kind": "message", "parts": [{"kind": "text", "text": "ok"}]}
            }))
        }
    });
    let app = Router::new().route("/", a2a.clone()).route("/jsonrpc", a2a);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), calls)
}

/// A finished flow belonging to `victim`, and the traceparent that names it.
async fn finished_flow_of(
    server: &common::TestServer,
    victim: Uuid,
    root_agent: Uuid,
) -> (String, String) {
    let (flow_id, traceparent) = common::open_flow(&server.db, victim, root_agent).await;
    sqlx::query("UPDATE flows SET status = 'completed', completed_at = now() WHERE flow_id = $1")
        .bind(&flow_id)
        .execute(&server.db)
        .await
        .unwrap();
    (flow_id, traceparent)
}

/// `(owner, status)` of a flow row.
async fn flow_row(server: &common::TestServer, flow_id: &str) -> (Uuid, String) {
    sqlx::query_as("SELECT user_id, status FROM flows WHERE flow_id = $1")
        .bind(flow_id)
        .fetch_one(&server.db)
        .await
        .unwrap()
}

async fn is_participant(server: &common::TestServer, flow_id: &str, agent: Uuid) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM flow_participants WHERE flow_id = $1 AND agent_id = $2)",
    )
    .bind(flow_id)
    .bind(agent)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

#[tokio::test]
#[serial]
async fn agent_proxy_refuses_a_traceparent_naming_another_users_flow() {
    let server = common::TestServer::start().await;
    let victim = init_admin(&server).await;
    let caller = seed_member(&server, "mallory").await;
    let (stub_url, calls) = start_stub_agent().await;
    let victims_agent = seed_public_agent(&server, victim, "own-agent", &stub_url).await;
    let target = seed_public_agent(&server, victim, "target-agent", &stub_url).await;
    let (flow_id, traceparent) = finished_flow_of(&server, victim, victims_agent).await;

    let res = common::as_member(
        server
            .client
            .post(server.url(&format!("/api/agents/{target}")))
            .header("traceparent", &traceparent),
        &caller.to_string(),
        "mallory",
    )
    .json(&json!({"jsonrpc": "2.0", "method": "message/send", "id": "1", "params": {
        "message": {"messageId": "m1", "role": "user", "parts": [{"kind": "text", "text": "hi"}]}
    }}))
    .send()
    .await
    .unwrap();

    assert_eq!(res.status(), 403);
    assert_eq!(
        flow_row(&server, &flow_id).await,
        (victim, "completed".to_string()),
        "the victim's flow must keep its owner and must not be re-opened"
    );
    assert!(
        !is_participant(&server, &flow_id, target).await,
        "the caller's chosen agent must not become a participant of the victim's flow"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing may be forwarded to the agent"
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_proxy_still_accepts_the_users_own_traceparent() {
    let server = common::TestServer::start().await;
    let owner = init_admin(&server).await;
    let (stub_url, calls) = start_stub_agent().await;
    let root = seed_public_agent(&server, owner, "own-root", &stub_url).await;
    let target = seed_public_agent(&server, owner, "own-target", &stub_url).await;
    let (flow_id, traceparent) = finished_flow_of(&server, owner, root).await;

    let res = common::as_superuser(
        server
            .client
            .post(server.url(&format!("/api/agents/{target}")))
            .header("traceparent", &traceparent),
        &owner.to_string(),
        "admin",
    )
    .json(&json!({"jsonrpc": "2.0", "method": "message/send", "id": "1", "params": {
        "message": {"messageId": "m1", "role": "user", "parts": [{"kind": "text", "text": "hi"}]}
    }}))
    .send()
    .await
    .unwrap();

    assert!(res.status().is_success(), "{}", res.status());
    assert_eq!(
        flow_row(&server, &flow_id).await.0,
        owner,
        "the flow stays the user's own"
    );
    assert!(is_participant(&server, &flow_id, target).await);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    server.cleanup().await;
}

// ─── The shared helpers the dispatch handlers use ────────────────────────────

#[tokio::test]
#[serial]
async fn opening_a_flow_never_succeeds_for_another_users_flow() {
    let server = common::TestServer::start().await;
    let victim = init_admin(&server).await;
    let caller = seed_member(&server, "mallory").await;
    let (stub_url, _) = start_stub_agent().await;
    let agent = seed_public_agent(&server, victim, "helper-agent", &stub_url).await;
    let (flow_id, _) = finished_flow_of(&server, victim, agent).await;
    let metadata = json!({});
    let new_flow = |user_id| nasiko_server::flows::NewFlow {
        flow_id: &flow_id,
        user_id,
        root_agent_id: Some(agent),
        root_agent_name: "helper-agent",
        title: Some("hello"),
        metadata: &metadata,
        session_id: None,
    };

    assert!(
        nasiko_server::flows::flow_belongs_to_another(&server.db, &flow_id, caller)
            .await
            .unwrap()
    );
    assert!(
        !nasiko_server::flows::flow_belongs_to_another(&server.db, &flow_id, victim)
            .await
            .unwrap()
    );
    assert!(
        !nasiko_server::flows::flow_belongs_to_another(&server.db, "no-such-flow", caller)
            .await
            .unwrap(),
        "a flow that does not exist yet belongs to nobody"
    );

    let refused = nasiko_server::flows::open_flow(&server.db, new_flow(caller)).await;
    assert!(
        matches!(
            refused,
            Err(nasiko_server::flows::OpenFlowError::ForeignFlow)
        ),
        "{refused:?}"
    );
    assert_eq!(
        flow_row(&server, &flow_id).await,
        (victim, "completed".to_string()),
        "a refused open must not touch the row"
    );

    // The owner re-opens their own flow: running again, completion cleared.
    nasiko_server::flows::open_flow(&server.db, new_flow(victim))
        .await
        .expect("the owner may re-open their own flow");
    assert_eq!(
        flow_row(&server, &flow_id).await,
        (victim, "running".to_string())
    );

    // A flow id nobody has used yet is created for the caller.
    let fresh = Uuid::new_v4().simple().to_string();
    nasiko_server::flows::open_flow(
        &server.db,
        nasiko_server::flows::NewFlow {
            flow_id: &fresh,
            user_id: caller,
            root_agent_id: None,
            root_agent_name: "orchestrator",
            title: None,
            metadata: &metadata,
            session_id: None,
        },
    )
    .await
    .expect("a new flow opens");
    assert_eq!(
        flow_row(&server, &fresh).await,
        (caller, "running".to_string())
    );

    server.cleanup().await;
}
