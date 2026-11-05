//! `/api/agent-state/*`: per-agent durable tasks and key-value state.
//!
//! The routes sit outside the session auth. A deployed agent presents its state-scoped
//! credential (`NASIKO_AGENT_TOKEN`), every row is scoped by the agent behind that credential, writes carry optimistic versions, rows expire, and what is stored is ciphertext.
//! These tests drive the real HTTP surface and look at the raw tables where the claim is about
//! storage.
//!
//! Needs infra up (Postgres, Redis, S3):
//!   TEST_PG_URL=... TEST_REDIS_URL=... TEST_S3_ENDPOINT=... \
//!     cargo test -p nasiko-server --test agent_state -- --test-threads=1

mod common;

use std::time::Duration;

use chrono::{DateTime, Utc};
use common::TestServer;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Method, RequestBuilder, Response};
use serde_json::{Value, json};
use serial_test::serial;
use sqlx::PgPool;
use uuid::Uuid;

// ─── Fixtures ───────────────────────────────────────────────────────────────

struct Agent {
    id: Uuid,
    token: String,
}

async fn seed_user(server: &TestServer, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO users (username, email) VALUES ($1, $2) RETURNING id")
        .bind(name)
        .bind(format!("{name}@test.local"))
        .fetch_one(&server.db)
        .await
        .expect("seed user")
}

async fn seed_agent_row(server: &TestServer, owner_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, image, status) \
         VALUES ($1, $2, 'nasiko/echo:1.0.0', 'running') RETURNING id",
    )
    .bind(name)
    .bind(owner_id)
    .fetch_one(&server.db)
    .await
    .expect("seed agent")
}

async fn mint_state_token(server: &TestServer, agent_id: Uuid) -> String {
    nasiko_server::agent_state::mint_state_token(&server.db, agent_id)
        .await
        .expect("mint state token")
}

/// An agent with its own owner and a freshly minted state credential.
async fn new_agent(server: &TestServer, name: &str) -> Agent {
    let owner = seed_user(server, &format!("{name}-owner")).await;
    let id = seed_agent_row(server, owner, name).await;
    let token = mint_state_token(server, id).await;
    Agent { id, token }
}

fn url(server: &TestServer, path: &str) -> String {
    server.url(&format!("/api/agent-state{path}"))
}

fn call(server: &TestServer, agent: &Agent, method: Method, path: &str) -> RequestBuilder {
    server
        .client
        .request(method, url(server, path))
        .bearer_auth(&agent.token)
}

/// A path segment as a client must send it: every byte that is not alphanumeric escaped.
fn enc(segment: &str) -> String {
    utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string()
}

async fn put_task_in(
    server: &TestServer,
    agent: &Agent,
    task_id: &str,
    context_id: Option<&str>,
    state: &str,
    body: Value,
) -> Response {
    call(
        server,
        agent,
        Method::PUT,
        &format!("/tasks/{}", enc(task_id)),
    )
    .json(&json!({"context_id": context_id, "state": state, "body": body}))
    .send()
    .await
    .unwrap()
}

async fn put_task(server: &TestServer, agent: &Agent, task_id: &str, body: Value) -> Response {
    put_task_in(server, agent, task_id, None, "working", body).await
}

async fn get_task(server: &TestServer, agent: &Agent, task_id: &str) -> Response {
    call(
        server,
        agent,
        Method::GET,
        &format!("/tasks/{}", enc(task_id)),
    )
    .send()
    .await
    .unwrap()
}

async fn delete_task(server: &TestServer, agent: &Agent, task_id: &str) -> Response {
    call(
        server,
        agent,
        Method::DELETE,
        &format!("/tasks/{}", enc(task_id)),
    )
    .send()
    .await
    .unwrap()
}

async fn put_kv(server: &TestServer, agent: &Agent, key: &str, value: Value) -> Response {
    call(server, agent, Method::PUT, &format!("/kv/{}", enc(key)))
        .json(&json!({ "value": value }))
        .send()
        .await
        .unwrap()
}

async fn get_kv(server: &TestServer, agent: &Agent, key: &str) -> Response {
    call(server, agent, Method::GET, &format!("/kv/{}", enc(key)))
        .send()
        .await
        .unwrap()
}

async fn delete_kv(server: &TestServer, agent: &Agent, key: &str) -> Response {
    call(server, agent, Method::DELETE, &format!("/kv/{}", enc(key)))
        .send()
        .await
        .unwrap()
}

/// A listing request that must succeed; returns the page.
async fn list(server: &TestServer, agent: &Agent, path_and_query: &str) -> Value {
    let res = call(server, agent, Method::GET, path_and_query)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "GET {path_and_query}");
    res.json().await.unwrap()
}

/// The ids (or keys) of a listing's items, in order.
fn names(page: &Value, field: &str) -> Vec<String> {
    page["items"]
        .as_array()
        .expect("items is an array")
        .iter()
        .map(|item| {
            item[field]
                .as_str()
                .expect("item names its row")
                .to_string()
        })
        .collect()
}

/// Asserts an error response's status and its stable `code`, and returns the body.
async fn expect_error(res: Response, status: u16, code: &str) -> Value {
    assert_eq!(res.status(), status, "status");
    let body: Value = res.json().await.expect("error bodies are JSON");
    assert_eq!(body["code"], code, "code in {body}");
    assert!(body["error"].is_string(), "error message in {body}");
    body
}

fn timestamp(body: &Value, field: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(body[field].as_str().expect("timestamp is a string"))
        .expect("timestamp is RFC 3339")
        .with_timezone(&Utc)
}

async fn expire_task(server: &TestServer, agent: &Agent, task_id: &str) {
    sqlx::query(
        "UPDATE agent_tasks SET expires_at = now() - interval '1 second' \
         WHERE agent_id = $1 AND task_id = $2",
    )
    .bind(agent.id)
    .bind(task_id)
    .execute(&server.db)
    .await
    .unwrap();
}

async fn expire_kv(server: &TestServer, agent: &Agent, key: &str) {
    sqlx::query(
        "UPDATE agent_kv SET expires_at = now() - interval '1 second' \
         WHERE agent_id = $1 AND key = $2",
    )
    .bind(agent.id)
    .bind(key)
    .execute(&server.db)
    .await
    .unwrap();
}

/// Rows that hold state, leaving out tombstones.
async fn count_live_rows(server: &TestServer, table: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {table} WHERE deleted_at IS NULL"
    ))
    .fetch_one(&server.db)
    .await
    .unwrap()
}

async fn count_rows(server: &TestServer, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&server.db)
        .await
        .unwrap()
}

/// A second pool onto the test's database. The server's own pool is a single connection, so a
/// test that needs to hold a lock the way a concurrent writer would has to bring its own.
async fn peer_pool(server: &TestServer) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with((*server.db.connect_options()).clone())
        .await
        .expect("connect peer pool")
}

/// A document shaped like a serialized A2A task, with the awkward corners of JSON in it.
fn sample_task_body() -> Value {
    json!({
        "id": "task-1",
        "status": {"state": "input-required", "timestamp": "2026-10-07T09:00:00Z"},
        "history": [
            {"role": "user", "parts": [{"kind": "text", "text": "héllo ✓ \u{1F600} \"quoted\" \\ slash"}]},
            {"role": "agent", "parts": [{"kind": "data", "data": {"n": 1, "f": 1.5, "big": 9007199254740993_i64}}]}
        ],
        "metadata": {"nil": null, "flag": false, "nested": {"deep": [1, [2, [3, {}]]]}},
        "empty_object": {},
        "empty_array": []
    })
}

// ─── Authentication ─────────────────────────────────────────────────────────

/// Every route and method the API serves, as `(method, path)`.
fn every_route() -> Vec<(Method, &'static str)> {
    vec![
        (Method::POST, "/register"),
        (Method::GET, "/tasks"),
        (Method::PUT, "/tasks/t1"),
        (Method::GET, "/tasks/t1"),
        (Method::DELETE, "/tasks/t1"),
        (Method::GET, "/kv"),
        (Method::PUT, "/kv/k1"),
        (Method::GET, "/kv/k1"),
        (Method::DELETE, "/kv/k1"),
    ]
}

#[tokio::test]
#[serial]
async fn no_route_serves_a_request_without_a_credential() {
    let server = TestServer::start().await;
    for (method, path) in every_route() {
        let res = server
            .client
            .request(method.clone(), url(&server, path))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        expect_error(res, 401, "unauthorized").await;
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn only_a_live_agent_credential_authenticates() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "auth-agent").await;

    for bad in ["", "not-a-token", "ngt_deadbeef", &"A".repeat(5000)] {
        let res = server
            .client
            .get(url(&server, "/tasks"))
            .bearer_auth(bad)
            .send()
            .await
            .unwrap();
        expect_error(res, 401, "unauthorized").await;
    }

    // The right secret under the wrong scheme is no credential at all.
    let basic = server
        .client
        .get(url(&server, "/tasks"))
        .basic_auth("agent", Some(&agent.token))
        .send()
        .await
        .unwrap();
    expect_error(basic, 401, "unauthorized").await;

    let ok = call(&server, &agent, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_user_session_token_is_not_an_agent_credential() {
    let server = TestServer::start().await;
    let owner = seed_user(&server, "session-owner").await;
    let res = common::as_superuser(
        server.client.get(url(&server, "/tasks")),
        &owner.to_string(),
        "admin",
    )
    .send()
    .await
    .unwrap();
    expect_error(res, 401, "unauthorized").await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_revoked_credential_stops_authenticating() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "revoked-agent").await;
    let ok = call(&server, &agent, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    nasiko_mcp_gateway::agent_tokens::revoke_scoped(
        &server.db,
        agent.id,
        nasiko_mcp_gateway::agent_tokens::TokenScope::State,
    )
    .await
    .unwrap();

    let res = call(&server, &agent, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    expect_error(res, 401, "unauthorized").await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_mcp_gateway_credential_does_not_open_the_state_api() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "gateway-token-agent").await;
    // The gateway credential also travels in URL paths, so it must not double as this one.
    let gateway_token = common::mint_gateway_token(&server.db, agent.id).await;
    for (method, path) in every_route() {
        let res = server
            .client
            .request(method, url(&server, path))
            .bearer_auth(&gateway_token)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        expect_error(res, 401, "unauthorized").await;
    }
    // The agent's own state credential is unaffected by the gateway one existing.
    let ok = call(&server, &agent, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_state_credential_does_not_open_the_mcp_gateway() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "state-token-agent").await;
    let res = server
        .client
        .post(server.url("/api/mcp"))
        .bearer_auth(&agent.token)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_gateway_credential_table_stays_as_servers_before_the_state_api_use_it() {
    // During a rolling upgrade older servers still mint and authenticate gateway credentials
    // against `agent_gateway_tokens` with no notion of a state credential. Its key must stay
    // exactly (agent_id) so their upsert keeps working, and the state credential must live
    // elsewhere so their scope-blind lookup can never accept it.
    let server = TestServer::start().await;
    let agent = new_agent(&server, "rolling-upgrade-agent").await;

    let key_columns: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname::text FROM pg_index i
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
         WHERE i.indrelid = 'agent_gateway_tokens'::regclass AND i.indisprimary
         ORDER BY a.attnum",
    )
    .fetch_all(&server.db)
    .await
    .unwrap();
    assert_eq!(key_columns, ["agent_id"]);

    // The statement an older server's mint runs, verbatim in shape: insert, then rotate.
    for hash in ["old-server-hash-1", "old-server-hash-2"] {
        sqlx::query(
            "INSERT INTO agent_gateway_tokens (agent_id, token_hash)
             VALUES ($1, $2)
             ON CONFLICT (agent_id) DO UPDATE SET
                 token_hash = EXCLUDED.token_hash,
                 prev_token_hash = CASE
                     WHEN agent_gateway_tokens.revoked_at IS NULL
                     THEN agent_gateway_tokens.token_hash
                 END,
                 rotated_at = now(),
                 created_at = now(),
                 revoked_at = NULL",
        )
        .bind(agent.id)
        .bind(hash)
        .execute(&server.db)
        .await
        .expect("an older server's gateway mint still succeeds");
    }

    // An older server's scope-blind gateway lookup: every live hash in the gateway table.
    let gateway_hashes: Vec<String> =
        sqlx::query_scalar("SELECT token_hash FROM agent_gateway_tokens WHERE revoked_at IS NULL")
            .fetch_all(&server.db)
            .await
            .unwrap();
    let state_hash = nasiko_mcp_gateway::agent_tokens::hash_token(&agent.token);
    assert!(
        !gateway_hashes.contains(&state_hash),
        "a state credential must never be in the gateway table"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn each_credential_authenticates_only_for_its_own_scope() {
    use nasiko_mcp_gateway::agent_tokens::{self, TokenScope};

    let server = TestServer::start().await;
    let agent = new_agent(&server, "two-door-agent").await;
    let gateway_token = common::mint_gateway_token(&server.db, agent.id).await;

    let state_at_gateway = agent_tokens::authenticate(&server.db, &agent.token)
        .await
        .unwrap();
    assert_eq!(state_at_gateway, None);
    let gateway_at_state =
        agent_tokens::authenticate_scoped(&server.db, &gateway_token, TokenScope::State)
            .await
            .unwrap();
    assert_eq!(gateway_at_state, None);

    let own_doors = (
        agent_tokens::authenticate(&server.db, &gateway_token)
            .await
            .unwrap(),
        agent_tokens::authenticate_scoped(&server.db, &agent.token, TokenScope::State)
            .await
            .unwrap(),
    );
    assert_eq!(own_doors, (Some(agent.id), Some(agent.id)));
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn minting_again_rotates_the_state_credential_and_leaves_the_gateway_one_alone() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "rotating-agent").await;
    let gateway_token = common::mint_gateway_token(&server.db, agent.id).await;

    let rotated = Agent {
        id: agent.id,
        token: mint_state_token(&server, agent.id).await,
    };
    assert_ne!(rotated.token, agent.token);
    assert!(rotated.token.starts_with("nst_"), "{}", rotated.token);
    let res = call(&server, &rotated, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // Rotating the state credential is not a rotation of the gateway one.
    let agent_id: Option<Uuid> =
        nasiko_mcp_gateway::agent_tokens::authenticate(&server.db, &gateway_token)
            .await
            .unwrap();
    assert_eq!(agent_id, Some(agent.id));
    server.cleanup().await;
}

// ─── Isolation between agents ───────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn an_agent_cannot_see_or_touch_another_agents_tasks() {
    let server = TestServer::start().await;
    let alice = new_agent(&server, "alice").await;
    let bob = new_agent(&server, "bob").await;
    let alice_body = json!({"owner": "alice"});
    assert_eq!(
        put_task(&server, &alice, "shared-id", alice_body.clone())
            .await
            .status(),
        200
    );

    // Bob asks for Alice's task by its exact id: absent, not forbidden.
    expect_error(get_task(&server, &bob, "shared-id").await, 404, "not_found").await;
    expect_error(
        delete_task(&server, &bob, "shared-id").await,
        404,
        "not_found",
    )
    .await;
    assert!(names(&list(&server, &bob, "/tasks").await, "task_id").is_empty());

    // Bob writing the same id makes his own row; it neither replaces nor versions hers.
    let bobs = put_task(&server, &bob, "shared-id", json!({"owner": "bob"})).await;
    assert_eq!(bobs.status(), 200);
    assert_eq!(bobs.json::<Value>().await.unwrap()["version"], 1);

    let hers: Value = get_task(&server, &alice, "shared-id")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(hers["body"], alice_body);
    assert_eq!(hers["version"], 1);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn an_agent_cannot_see_or_touch_another_agents_kv_entries() {
    let server = TestServer::start().await;
    let alice = new_agent(&server, "alice-kv").await;
    let bob = new_agent(&server, "bob-kv").await;
    put_kv(&server, &alice, "shared-key", json!("alice")).await;

    expect_error(get_kv(&server, &bob, "shared-key").await, 404, "not_found").await;
    expect_error(
        delete_kv(&server, &bob, "shared-key").await,
        404,
        "not_found",
    )
    .await;
    assert!(names(&list(&server, &bob, "/kv").await, "key").is_empty());

    let bobs = put_kv(&server, &bob, "shared-key", json!("bob")).await;
    assert_eq!(bobs.json::<Value>().await.unwrap()["version"], 1);
    let hers: Value = get_kv(&server, &alice, "shared-key")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(hers["value"], "alice");

    server.cleanup().await;
}

// ─── Tasks ──────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn a_task_is_created_replaced_read_back_and_deleted() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "task-agent").await;
    let body = sample_task_body();

    let created = put_task_in(
        &server,
        &agent,
        "task-1",
        Some("ctx-1"),
        "working",
        body.clone(),
    )
    .await;
    assert_eq!(created.status(), 200);
    let created: Value = created.json().await.unwrap();
    assert_eq!(created["task_id"], "task-1");
    assert_eq!(created["version"], 1);
    let mut put_fields: Vec<&str> = created
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    put_fields.sort_unstable();
    assert_eq!(
        put_fields,
        ["expires_at", "task_id", "updated_at", "version"]
    );

    // What is read back is exactly what was written.
    let read: Value = get_task(&server, &agent, "task-1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read["body"], body);
    assert_eq!(read["context_id"], "ctx-1");
    assert_eq!(read["state"], "working");
    assert_eq!(read["version"], 1);
    assert_eq!(read["updated_at"], created["updated_at"]);
    assert_eq!(read["expires_at"], created["expires_at"]);
    let mut get_fields: Vec<&str> = read
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    get_fields.sort_unstable();
    assert_eq!(
        get_fields,
        [
            "body",
            "context_id",
            "expires_at",
            "state",
            "task_id",
            "updated_at",
            "version"
        ]
    );

    // A second write replaces the first, one version up, with no merging.
    let replacement = json!({"id": "task-1", "status": {"state": "completed"}});
    let replaced = put_task_in(
        &server,
        &agent,
        "task-1",
        None,
        "completed",
        replacement.clone(),
    )
    .await;
    let replaced: Value = replaced.json().await.unwrap();
    assert_eq!(replaced["version"], 2);
    assert!(timestamp(&replaced, "updated_at") >= timestamp(&created, "updated_at"));
    let read: Value = get_task(&server, &agent, "task-1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read["body"], replacement);
    assert_eq!(read["state"], "completed");
    assert_eq!(read["context_id"], Value::Null);
    assert_eq!(read["version"], 2);

    assert_eq!(delete_task(&server, &agent, "task-1").await.status(), 204);
    expect_error(get_task(&server, &agent, "task-1").await, 404, "not_found").await;
    expect_error(
        delete_task(&server, &agent, "task-1").await,
        404,
        "not_found",
    )
    .await;

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_null_body_is_a_value_like_any_other() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "null-body-agent").await;
    assert_eq!(
        put_task(&server, &agent, "t", Value::Null).await.status(),
        200
    );
    let read: Value = get_task(&server, &agent, "t").await.json().await.unwrap();
    assert_eq!(read["body"], Value::Null);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn tasks_list_newest_first_and_page_by_cursor() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "list-agent").await;
    for (id, context, state) in [
        ("p1", "ctx-a", "working"),
        ("p2", "ctx-a", "working"),
        ("p3", "ctx-a", "completed"),
        ("p4", "ctx-a", "working"),
        ("p5", "ctx-a", "working"),
        ("q1", "ctx-b", "working"),
        ("q2", "ctx-b", "working"),
    ] {
        let res = put_task_in(&server, &agent, id, Some(context), state, json!({"id": id})).await;
        assert_eq!(res.status(), 200);
    }

    let all = list(&server, &agent, "/tasks").await;
    assert_eq!(
        names(&all, "task_id"),
        ["q2", "q1", "p5", "p4", "p3", "p2", "p1"],
        "newest write first"
    );
    assert_eq!(all["next_cursor"], Value::Null);
    assert_eq!(
        all["items"][0]["body"],
        json!({"id": "q2"}),
        "items carry their body"
    );

    // Three pages of the context, two rows at a time.
    let page1 = list(&server, &agent, "/tasks?context_id=ctx-a&limit=2").await;
    assert_eq!(names(&page1, "task_id"), ["p5", "p4"]);
    let cursor1 = page1["next_cursor"]
        .as_str()
        .expect("more follow")
        .to_string();
    let page2 = list(
        &server,
        &agent,
        &format!("/tasks?context_id=ctx-a&limit=2&cursor={cursor1}"),
    )
    .await;
    assert_eq!(names(&page2, "task_id"), ["p3", "p2"]);
    let cursor2 = page2["next_cursor"]
        .as_str()
        .expect("one more follows")
        .to_string();
    let page3 = list(
        &server,
        &agent,
        &format!("/tasks?context_id=ctx-a&limit=2&cursor={cursor2}"),
    )
    .await;
    assert_eq!(names(&page3, "task_id"), ["p1"]);
    assert_eq!(page3["next_cursor"], Value::Null, "the last page says so");

    // A context that fits the page exactly has no empty page after it.
    let exact = list(&server, &agent, "/tasks?context_id=ctx-b&limit=2").await;
    assert_eq!(names(&exact, "task_id"), ["q2", "q1"]);
    assert_eq!(exact["next_cursor"], Value::Null);

    // Filters combine, and paging works through them.
    let working = list(
        &server,
        &agent,
        "/tasks?context_id=ctx-a&state=working&limit=3",
    )
    .await;
    assert_eq!(names(&working, "task_id"), ["p5", "p4", "p2"]);
    let cursor = working["next_cursor"]
        .as_str()
        .expect("p1 follows")
        .to_string();
    let rest = list(
        &server,
        &agent,
        &format!("/tasks?context_id=ctx-a&state=working&limit=3&cursor={cursor}"),
    )
    .await;
    assert_eq!(names(&rest, "task_id"), ["p1"]);
    let completed = list(&server, &agent, "/tasks?state=completed").await;
    assert_eq!(names(&completed, "task_id"), ["p3"]);
    assert!(
        names(
            &list(&server, &agent, "/tasks?context_id=nobody").await,
            "task_id"
        )
        .is_empty()
    );

    server.cleanup().await;
}

/// Follows `path`'s cursors to the end and returns every page.
async fn all_pages(server: &TestServer, agent: &Agent, path: &str) -> Vec<Value> {
    let mut pages = vec![list(server, agent, path).await];
    while let Some(cursor) = pages.last().unwrap()["next_cursor"].as_str() {
        let separator = if path.contains('?') { '&' } else { '?' };
        let next = format!("{path}{separator}cursor={cursor}");
        pages.push(list(server, agent, &next).await);
    }
    pages
}

#[tokio::test]
#[serial]
async fn a_listing_of_large_bodies_pages_in_short_pages_within_the_byte_budget() {
    const MIB: usize = 1024 * 1024;
    let server = TestServer::start().await;
    let agent = new_agent(&server, "big-list-agent").await;
    // 100 bodies of exactly 1 MiB as sent: a 100-item page of them would be 100 MiB.
    let body = format!("\"{}\"", "x".repeat(MIB - 2));
    for n in 0..100 {
        let res = put_task_raw(&server, &agent, &format!("t{n:03}"), &body).await;
        assert_eq!(res.status(), 200, "task {n}");
    }

    let pages = all_pages(&server, &agent, "/tasks?limit=100").await;
    let mut seen = Vec::new();
    for page in &pages {
        let ids = names(page, "task_id");
        assert!(!ids.is_empty(), "every page carries something");
        assert!(
            ids.len() <= 8,
            "8 MiB holds at most 8 of them, got {}",
            ids.len()
        );
        seen.extend(ids);
    }
    assert!(
        pages.len() >= 13,
        "100 MiB in 8 MiB pages, got {} pages",
        pages.len()
    );
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 100, "every task exactly once");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_listing_byte_budget_comes_from_configuration_and_a_larger_row_lists_alone() {
    let server = TestServer::start_with(|c| c.agent_state_list_bytes_budget = 25).await;
    let agent = new_agent(&server, "budget-agent").await;
    // Each value is 10 bytes as sent, except `c`, which alone is over the budget.
    for (key, value) in [("a", "12345678"), ("b", "12345678"), ("d", "12345678")] {
        put_kv(&server, &agent, key, json!(value)).await;
    }
    put_kv(&server, &agent, "c", json!("x".repeat(40))).await;

    let pages: Vec<Vec<String>> = all_pages(&server, &agent, "/kv")
        .await
        .iter()
        .map(|page| names(page, "key"))
        .collect();
    assert_eq!(pages, [vec!["a", "b"], vec!["c"], vec!["d"]]);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_row_that_does_not_decrypt_is_left_out_of_listings_and_fails_only_its_own_read() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "damaged-agent").await;
    for id in ["a", "b", "c"] {
        put_task(&server, &agent, id, json!(id)).await;
        put_kv(&server, &agent, id, json!(id)).await;
    }
    for table in ["agent_tasks", "agent_kv"] {
        let (column, name) = if table == "agent_tasks" {
            ("body", "task_id")
        } else {
            ("value", "key")
        };
        sqlx::query(&format!(
            "UPDATE {table} SET {column} = 'bm90IGNpcGhlcnRleHQ=' WHERE {name} = 'b'"
        ))
        .execute(&server.db)
        .await
        .unwrap();
    }

    let mut tasks = names(&list(&server, &agent, "/tasks").await, "task_id");
    tasks.sort();
    assert_eq!(tasks, ["a", "c"]);
    assert_eq!(
        names(&list(&server, &agent, "/kv").await, "key"),
        ["a", "c"]
    );
    // One row per page: the damaged one yields an empty page that still moves on past it.
    let pages: Vec<Vec<String>> = all_pages(&server, &agent, "/kv?limit=1")
        .await
        .iter()
        .map(|page| names(page, "key"))
        .collect();
    assert_eq!(pages, [vec!["a"], vec![], vec!["c"]]);

    expect_error(get_task(&server, &agent, "b").await, 500, "internal_error").await;
    expect_error(get_kv(&server, &agent, "b").await, 500, "internal_error").await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn rewriting_a_task_moves_it_to_the_front() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "reorder-agent").await;
    for id in ["a", "b", "c"] {
        put_task(&server, &agent, id, json!(id)).await;
    }
    put_task(&server, &agent, "a", json!("again")).await;
    let page = list(&server, &agent, "/tasks").await;
    assert_eq!(names(&page, "task_id"), ["a", "c", "b"]);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn tasks_stamped_in_the_same_instant_page_in_id_order_without_gaps_or_repeats() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "tie-agent").await;
    for id in ["t-c", "t-a", "t-b", "t-e", "t-d"] {
        put_task(&server, &agent, id, json!(id)).await;
    }
    sqlx::query("UPDATE agent_tasks SET updated_at = '2026-01-01T00:00:00Z' WHERE agent_id = $1")
        .bind(agent.id)
        .execute(&server.db)
        .await
        .unwrap();
    put_task(&server, &agent, "t-newest", json!("n")).await;

    let mut seen = Vec::new();
    let mut path = "/tasks?limit=2".to_string();
    loop {
        let page = list(&server, &agent, &path).await;
        seen.extend(names(&page, "task_id"));
        match page["next_cursor"].as_str() {
            Some(cursor) => path = format!("/tasks?limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(seen, ["t-newest", "t-a", "t-b", "t-c", "t-d", "t-e"]);
    server.cleanup().await;
}

// ─── Key-value ──────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn a_kv_entry_is_created_replaced_read_back_and_deleted() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "kv-agent").await;
    let value = sample_task_body();

    let created = put_kv(&server, &agent, "config", value.clone()).await;
    assert_eq!(created.status(), 200);
    let created: Value = created.json().await.unwrap();
    assert_eq!(created["key"], "config");
    assert_eq!(created["version"], 1);
    let mut put_fields: Vec<&str> = created
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    put_fields.sort_unstable();
    assert_eq!(put_fields, ["expires_at", "key", "updated_at", "version"]);

    let read: Value = get_kv(&server, &agent, "config")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read["value"], value);
    assert_eq!(read["version"], 1);
    let mut get_fields: Vec<&str> = read
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    get_fields.sort_unstable();
    assert_eq!(
        get_fields,
        ["expires_at", "key", "updated_at", "value", "version"]
    );

    let replaced: Value = put_kv(&server, &agent, "config", json!(42))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(replaced["version"], 2);
    let read: Value = get_kv(&server, &agent, "config")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read["value"], 42);

    assert_eq!(delete_kv(&server, &agent, "config").await.status(), 204);
    expect_error(get_kv(&server, &agent, "config").await, 404, "not_found").await;
    expect_error(delete_kv(&server, &agent, "config").await, 404, "not_found").await;

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn any_json_value_can_be_a_kv_value() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "kv-json-agent").await;
    for (key, value) in [
        ("null", Value::Null),
        ("bool", json!(true)),
        ("int", json!(-7)),
        ("float", json!(2.5)),
        ("string", json!("text")),
        ("array", json!([1, "two", null])),
        ("object", json!({"a": {"b": []}})),
    ] {
        assert_eq!(
            put_kv(&server, &agent, key, value.clone()).await.status(),
            200
        );
        let read: Value = get_kv(&server, &agent, key).await.json().await.unwrap();
        assert_eq!(read["value"], value, "key {key}");
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn kv_entries_list_in_key_order_filter_by_prefix_and_page() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "kv-list-agent").await;
    for key in ["app/d", "other/x", "app/a", "app/c", "app/b"] {
        put_kv(&server, &agent, key, json!(key)).await;
    }

    let all = list(&server, &agent, "/kv").await;
    assert_eq!(
        names(&all, "key"),
        ["app/a", "app/b", "app/c", "app/d", "other/x"]
    );
    assert_eq!(all["items"][0]["value"], "app/a", "items carry their value");

    let page1 = list(
        &server,
        &agent,
        &format!("/kv?prefix={}&limit=3", enc("app/")),
    )
    .await;
    assert_eq!(names(&page1, "key"), ["app/a", "app/b", "app/c"]);
    let cursor = page1["next_cursor"]
        .as_str()
        .expect("app/d follows")
        .to_string();
    let page2 = list(
        &server,
        &agent,
        &format!("/kv?prefix={}&limit=3&cursor={cursor}", enc("app/")),
    )
    .await;
    assert_eq!(names(&page2, "key"), ["app/d"]);
    assert_eq!(page2["next_cursor"], Value::Null);

    assert!(names(&list(&server, &agent, "/kv?prefix=missing").await, "key").is_empty());
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_prefix_is_literal_so_like_wildcards_in_it_match_only_themselves() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "kv-wildcard-agent").await;
    for key in ["100%_done", "1000 done", "100xxdone", "a\\b", "aXb"] {
        put_kv(&server, &agent, key, json!(1)).await;
    }
    let percent = list(&server, &agent, &format!("/kv?prefix={}", enc("100%_"))).await;
    assert_eq!(names(&percent, "key"), ["100%_done"]);
    let backslash = list(&server, &agent, &format!("/kv?prefix={}", enc("a\\"))).await;
    assert_eq!(names(&backslash, "key"), ["a\\b"]);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn keys_and_task_ids_with_reserved_characters_round_trip_when_percent_encoded() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "encoding-agent").await;
    let awkward = "a/b c?d#e%f+g&h=i é日本";
    assert_eq!(
        put_kv(&server, &agent, awkward, json!("v")).await.status(),
        200
    );
    let read: Value = get_kv(&server, &agent, awkward).await.json().await.unwrap();
    assert_eq!(read["key"], awkward);
    assert_eq!(names(&list(&server, &agent, "/kv").await, "key"), [awkward]);
    assert_eq!(delete_kv(&server, &agent, awkward).await.status(), 204);

    assert_eq!(
        put_task(&server, &agent, awkward, json!(1)).await.status(),
        200
    );
    let read: Value = get_task(&server, &agent, awkward)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read["task_id"], awkward);
    server.cleanup().await;
}

// ─── Optimistic concurrency (If-Match) ──────────────────────────────────────

fn put_task_if_match(
    server: &TestServer,
    agent: &Agent,
    task_id: &str,
    if_match: &str,
    body: Value,
) -> RequestBuilder {
    call(
        server,
        agent,
        Method::PUT,
        &format!("/tasks/{}", enc(task_id)),
    )
    .header("If-Match", if_match)
    .json(&json!({"context_id": null, "state": "working", "body": body}))
}

fn put_kv_if_match(
    server: &TestServer,
    agent: &Agent,
    key: &str,
    if_match: &str,
    value: Value,
) -> RequestBuilder {
    call(server, agent, Method::PUT, &format!("/kv/{}", enc(key)))
        .header("If-Match", if_match)
        .json(&json!({ "value": value }))
}

#[tokio::test]
#[serial]
async fn a_task_write_at_a_stale_version_is_refused_and_changes_nothing() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "stale-agent").await;
    put_task(&server, &agent, "t", json!({"n": 1})).await;

    // The version it is at: accepted, and the next one is 2.
    let matching = put_task_if_match(&server, &agent, "t", "1", json!({"n": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(matching.status(), 200);
    assert_eq!(matching.json::<Value>().await.unwrap()["version"], 2);

    // A writer that still believes it is at 1 loses, and is told where the row really is.
    let stale = put_task_if_match(&server, &agent, "t", "1", json!({"n": "lost"}))
        .send()
        .await
        .unwrap();
    let conflict = expect_error(stale, 412, "version_conflict").await;
    assert_eq!(conflict["error"], "version_conflict");
    assert_eq!(conflict["current_version"], 2);

    let read: Value = get_task(&server, &agent, "t").await.json().await.unwrap();
    assert_eq!(
        read["body"],
        json!({"n": 2}),
        "the refused write must not land"
    );
    assert_eq!(read["version"], 2);

    // The quoted form of the same version is the same precondition.
    let quoted = put_task_if_match(&server, &agent, "t", "\"2\"", json!({"n": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(quoted.status(), 200);

    // Without the header the write is unconditional, whatever version the row is at.
    let unconditional = put_task(&server, &agent, "t", json!({"n": 4})).await;
    assert_eq!(unconditional.json::<Value>().await.unwrap()["version"], 4);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn version_zero_means_only_if_absent_and_a_missing_row_fails_any_other_version() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "absent-agent").await;

    // Expecting a version of a row that is not there fails, and creates nothing.
    let phantom = put_task_if_match(&server, &agent, "t", "3", json!(1))
        .send()
        .await
        .unwrap();
    let conflict = expect_error(phantom, 412, "version_conflict").await;
    assert_eq!(conflict["current_version"], 0);
    expect_error(get_task(&server, &agent, "t").await, 404, "not_found").await;

    // Expecting absence creates it, once.
    let first = put_task_if_match(&server, &agent, "t", "0", json!("first"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.json::<Value>().await.unwrap()["version"], 1);
    let second = put_task_if_match(&server, &agent, "t", "0", json!("second"))
        .send()
        .await
        .unwrap();
    let conflict = expect_error(second, 412, "version_conflict").await;
    assert_eq!(conflict["current_version"], 1);
    let read: Value = get_task(&server, &agent, "t").await.json().await.unwrap();
    assert_eq!(read["body"], "first");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_task_delete_at_a_stale_version_is_refused_and_removes_nothing() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "delete-stale-agent").await;
    put_task(&server, &agent, "t", json!(1)).await;
    put_task(&server, &agent, "t", json!(2)).await;

    let stale = call(&server, &agent, Method::DELETE, "/tasks/t")
        .header("If-Match", "1")
        .send()
        .await
        .unwrap();
    let conflict = expect_error(stale, 412, "version_conflict").await;
    assert_eq!(conflict["current_version"], 2);
    assert_eq!(
        get_task(&server, &agent, "t").await.status(),
        200,
        "the row must survive"
    );

    let matching = call(&server, &agent, Method::DELETE, "/tasks/t")
        .header("If-Match", "2")
        .send()
        .await
        .unwrap();
    assert_eq!(matching.status(), 204);

    // Nothing left to delete: absent is a 404, whatever version was expected.
    let gone = call(&server, &agent, Method::DELETE, "/tasks/t")
        .header("If-Match", "2")
        .send()
        .await
        .unwrap();
    expect_error(gone, 404, "not_found").await;

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn kv_writes_and_deletes_honour_if_match_the_same_way() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "kv-match-agent").await;
    put_kv(&server, &agent, "k", json!(1)).await;

    let matching = put_kv_if_match(&server, &agent, "k", "1", json!(2))
        .send()
        .await
        .unwrap();
    assert_eq!(matching.json::<Value>().await.unwrap()["version"], 2);

    let stale = put_kv_if_match(&server, &agent, "k", "1", json!("lost"))
        .send()
        .await
        .unwrap();
    let conflict = expect_error(stale, 412, "version_conflict").await;
    assert_eq!(conflict["current_version"], 2);
    let read: Value = get_kv(&server, &agent, "k").await.json().await.unwrap();
    assert_eq!(read["value"], 2);

    let phantom = put_kv_if_match(&server, &agent, "other", "5", json!(1))
        .send()
        .await
        .unwrap();
    assert_eq!(
        expect_error(phantom, 412, "version_conflict").await["current_version"],
        0
    );
    let created = put_kv_if_match(&server, &agent, "other", "0", json!(1))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);

    let stale_delete = call(&server, &agent, Method::DELETE, "/kv/k")
        .header("If-Match", "1")
        .send()
        .await
        .unwrap();
    expect_error(stale_delete, 412, "version_conflict").await;
    assert_eq!(get_kv(&server, &agent, "k").await.status(), 200);
    let delete = call(&server, &agent, Method::DELETE, "/kv/k")
        .header("If-Match", "2")
        .send()
        .await
        .unwrap();
    assert_eq!(delete.status(), 204);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_malformed_if_match_is_a_bad_request_and_the_write_does_not_happen() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "bad-match-agent").await;
    put_task(&server, &agent, "t", json!("kept")).await;

    for bad in ["abc", "-1", "*", "W/\"1\"", "1, 2", "1.5"] {
        let put = put_task_if_match(&server, &agent, "t", bad, json!("overwritten"))
            .send()
            .await
            .unwrap();
        expect_error(put, 400, "invalid_request").await;
        let delete = call(&server, &agent, Method::DELETE, "/tasks/t")
            .header("If-Match", bad)
            .send()
            .await
            .unwrap();
        expect_error(delete, 400, "invalid_request").await;
    }
    let read: Value = get_task(&server, &agent, "t").await.json().await.unwrap();
    assert_eq!(read["body"], "kept");
    assert_eq!(read["version"], 1);

    server.cleanup().await;
}

// ─── Size and rate limits ───────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn a_task_body_over_the_cap_is_refused_and_one_at_the_cap_is_kept() {
    let server = TestServer::start_with(|c| c.agent_state_max_body_bytes = 1024).await;
    let agent = new_agent(&server, "cap-agent").await;

    // A JSON string of n characters encodes to n + 2 bytes.
    let at_cap = put_task(&server, &agent, "fits", json!("x".repeat(1022))).await;
    assert_eq!(at_cap.status(), 200);
    let over_cap = put_task(&server, &agent, "too-big", json!("x".repeat(1023))).await;
    expect_error(over_cap, 413, "payload_too_large").await;
    expect_error(get_task(&server, &agent, "too-big").await, 404, "not_found").await;

    // A request far over the cap is stopped before its body is parsed, in the same shape.
    let huge = put_task(&server, &agent, "huge", json!("x".repeat(200_000))).await;
    expect_error(huge, 413, "payload_too_large").await;
    expect_error(get_task(&server, &agent, "huge").await, 404, "not_found").await;

    // A refused replacement leaves the earlier version in place.
    let refused = put_task(&server, &agent, "fits", json!("y".repeat(5000))).await;
    expect_error(refused, 413, "payload_too_large").await;
    let read: Value = get_task(&server, &agent, "fits")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read["version"], 1);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_cap_above_the_frameworks_default_body_limit_is_honoured() {
    // axum buffers at most 2 MiB of a body unless told otherwise; the cap is the operator's to
    // raise, so a legal write under it must not be stopped by that default.
    const MIB: usize = 1024 * 1024;
    let server = TestServer::start_with(|c| c.agent_state_max_body_bytes = 4 * MIB).await;
    let agent = new_agent(&server, "big-cap-agent").await;

    let large = put_task(&server, &agent, "large", json!("x".repeat(3 * MIB))).await;
    assert_eq!(large.status(), 200);
    let read: Value = get_task(&server, &agent, "large")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read["body"].as_str().unwrap().len(), 3 * MIB);

    let too_large = put_kv(&server, &agent, "too-large", json!("x".repeat(4 * MIB))).await;
    expect_error(too_large, 413, "payload_too_large").await;

    server.cleanup().await;
}

/// A task PUT whose `body` is `raw_body`, sent exactly as given rather than re-serialized.
async fn put_task_raw(
    server: &TestServer,
    agent: &Agent,
    task_id: &str,
    raw_body: &str,
) -> Response {
    call(
        server,
        agent,
        Method::PUT,
        &format!("/tasks/{}", enc(task_id)),
    )
    .header("Content-Type", "application/json")
    .body(format!(
        r#"{{"context_id":null,"state":"working","body":{raw_body}}}"#
    ))
    .send()
    .await
    .unwrap()
}

/// A JSON string of `chars` copies of `é`, each sent as the six-byte escape `\u00e9`.
fn escaped_non_ascii(chars: usize) -> String {
    format!("\"{}\"", "\\u00e9".repeat(chars))
}

#[tokio::test]
#[serial]
async fn the_cap_counts_the_body_as_sent_whitespace_included() {
    let server = TestServer::start_with(|c| c.agent_state_max_body_bytes = 32).await;
    let agent = new_agent(&server, "whitespace-agent").await;
    // Both are `{"a":1}` (7 bytes) once compacted; as sent, one is at the cap and one is over it.
    let padded = |spaces: usize| format!("{{\"a\":{}1}}", " ".repeat(spaces));
    assert_eq!(padded(25).len(), 32);

    assert_eq!(
        put_task_raw(&server, &agent, "t", &padded(25))
            .await
            .status(),
        200
    );
    expect_error(
        put_task_raw(&server, &agent, "t", &padded(26)).await,
        413,
        "payload_too_large",
    )
    .await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_body_over_the_cap_as_sent_is_refused_even_when_it_would_compact_under_it() {
    // Each escaped character is 6 bytes as sent and 2 once decoded and re-encoded, so this body
    // is just over the 1 MiB cap as sent and about a third of it in any compact form. It is also
    // within the transport allowance, so it is the cap on the field that has to refuse it.
    const MIB: usize = 1024 * 1024;
    let server = TestServer::start().await;
    let agent = new_agent(&server, "escaped-wire-agent").await;
    let body = escaped_non_ascii((MIB + 4096) / 6);
    assert!(body.len() > MIB && body.len() < MIB + 8192);

    expect_error(
        put_task_raw(&server, &agent, "t", &body).await,
        413,
        "payload_too_large",
    )
    .await;
    expect_error(get_task(&server, &agent, "t").await, 404, "not_found").await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_large_escaped_body_under_the_cap_comes_back_exactly_as_sent() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "escaped-body-agent").await;
    // 900 KB as sent, under the default 1 MiB cap; the escapes must survive, not turn into UTF-8.
    let body = escaped_non_ascii(150_000);
    assert_eq!(body.len(), 900_002);

    assert_eq!(
        put_task_raw(&server, &agent, "t", &body).await.status(),
        200
    );
    let read = get_task(&server, &agent, "t").await.text().await.unwrap();
    assert!(
        read.contains(&format!("\"body\":{body},")),
        "the body must come back byte for byte"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_body_keeps_its_number_precision_key_order_and_formatting() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "opaque-agent").await;
    // Integers past u64, a float past f64's precision, keys out of order, a duplicate key, and
    // a client's own spacing: a parsed-and-re-encoded body would lose every one of these.
    let body = r#"{"zeta": 18446744073709551616123, "alpha":[1.000000000000000000001, -0.0e0],
  "mid":{"b":2,"a":1}, "dup":1, "dup":2, "s":"\u00e9\/"}"#;

    assert_eq!(put_task_raw(&server, &agent, "t", body).await.status(), 200);
    let read = get_task(&server, &agent, "t").await.text().await.unwrap();
    assert!(read.contains(&format!("\"body\":{body},")), "{read}");

    let listed = call(&server, &agent, Method::GET, "/tasks")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(listed.contains(&format!("\"body\":{body},")), "{listed}");

    let kv = call(&server, &agent, Method::PUT, "/kv/k")
        .header("Content-Type", "application/json")
        .body(format!(r#"{{"value":{body}}}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(kv.status(), 200);
    let read = get_kv(&server, &agent, "k").await.text().await.unwrap();
    assert!(read.contains(&format!("\"value\":{body},")), "{read}");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_kv_value_over_the_cap_is_refused_and_one_at_the_cap_is_kept() {
    let server = TestServer::start_with(|c| c.agent_state_max_body_bytes = 1024).await;
    let agent = new_agent(&server, "kv-cap-agent").await;
    assert_eq!(
        put_kv(&server, &agent, "fits", json!("x".repeat(1022)))
            .await
            .status(),
        200
    );
    expect_error(
        put_kv(&server, &agent, "big", json!("x".repeat(1023))).await,
        413,
        "payload_too_large",
    )
    .await;
    expect_error(
        put_kv(&server, &agent, "huge", json!("x".repeat(200_000))).await,
        413,
        "payload_too_large",
    )
    .await;
    expect_error(get_kv(&server, &agent, "big").await, 404, "not_found").await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn an_agent_over_its_per_minute_budget_is_told_to_slow_down() {
    let server = TestServer::start_with(|c| c.agent_state_rate_limit_per_minute = 5).await;
    let busy = new_agent(&server, "busy-agent").await;
    let quiet = new_agent(&server, "quiet-agent").await;

    for attempt in 1..=5 {
        let res = call(&server, &busy, Method::GET, "/tasks")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "request {attempt} is within the budget");
    }
    // Part of the minute window has gone by, so the wait named is what is left of it.
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    let over = call(&server, &busy, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    let retry_after: u64 = over.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (50..=58).contains(&retry_after),
        "Retry-After {retry_after} must be the rest of the window"
    );
    expect_error(over, 429, "rate_limited").await;

    // The budget is per agent: the same traffic from another agent is unaffected.
    let other = call(&server, &quiet, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 200);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn refused_credentials_do_not_spend_an_agents_budget() {
    let server = TestServer::start_with(|c| c.agent_state_rate_limit_per_minute = 3).await;
    let agent = new_agent(&server, "budget-agent").await;

    // Presenting someone's token and failing is not that agent's traffic.
    for _ in 0..10 {
        let res = server
            .client
            .get(url(&server, "/tasks"))
            .bearer_auth("ngt_wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
    }
    for _ in 0..3 {
        let res = call(&server, &agent, Method::GET, "/tasks")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }
    let over = call(&server, &agent, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    assert_eq!(over.status(), 429);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn an_address_over_its_allowance_is_refused_before_any_token_is_looked_up() {
    // One agent may make 2 requests a minute, so one address may make 20 (ten agents' worth).
    let server = TestServer::start_with(|c| c.agent_state_rate_limit_per_minute = 2).await;
    let agent = new_agent(&server, "crowded-agent").await;

    for attempt in 1..=20 {
        let res = server
            .client
            .get(url(&server, "/tasks"))
            .bearer_auth(format!("nst_guess_{attempt}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "guess {attempt} is still looked up");
    }
    // The address has spent its allowance: even a valid credential from it is refused unread.
    let refused = call(&server, &agent, Method::GET, "/tasks")
        .send()
        .await
        .unwrap();
    let retry_after: u64 = refused.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after), "{retry_after}");
    expect_error(refused, 429, "rate_limited").await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_method_a_route_does_not_take_is_a_json_405() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "method-agent").await;
    for (method, path) in [
        (Method::POST, "/tasks/t1"),
        (Method::PATCH, "/kv/k1"),
        (Method::DELETE, "/tasks"),
        (Method::GET, "/register"),
    ] {
        let res = call(&server, &agent, method.clone(), path)
            .send()
            .await
            .unwrap();
        assert!(
            res.headers().contains_key("allow"),
            "{method} {path} names the methods it does take"
        );
        expect_error(res, 405, "method_not_allowed").await;
    }
    // Without a credential the route's existence is not confirmed either way.
    let res = server
        .client
        .post(url(&server, "/tasks/t1"))
        .send()
        .await
        .unwrap();
    expect_error(res, 401, "unauthorized").await;
    server.cleanup().await;
}

// ─── Expiry ─────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn every_write_sets_the_expiry_from_the_configured_ttl() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "ttl-agent").await;
    let seven_days = chrono::Duration::days(7);

    let task: Value = put_task(&server, &agent, "t", json!(1))
        .await
        .json()
        .await
        .unwrap();
    let span = timestamp(&task, "expires_at") - timestamp(&task, "updated_at");
    assert!(
        span >= seven_days && span < seven_days + chrono::Duration::seconds(1),
        "span {span}"
    );
    let entry: Value = put_kv(&server, &agent, "k", json!(1))
        .await
        .json()
        .await
        .unwrap();
    let span = timestamp(&entry, "expires_at") - timestamp(&entry, "updated_at");
    assert!(
        span >= seven_days && span < seven_days + chrono::Duration::seconds(1),
        "span {span}"
    );

    // Reading does not extend it.
    let read: Value = get_task(&server, &agent, "t").await.json().await.unwrap();
    assert_eq!(read["expires_at"], task["expires_at"]);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_ttl_comes_from_configuration() {
    let server = TestServer::start_with(|c| c.agent_state_ttl_days = 2).await;
    let agent = new_agent(&server, "short-ttl-agent").await;
    let two_days = chrono::Duration::days(2);
    let task: Value = put_task(&server, &agent, "t", json!(1))
        .await
        .json()
        .await
        .unwrap();
    let span = timestamp(&task, "expires_at") - timestamp(&task, "updated_at");
    assert!(
        span >= two_days && span < two_days + chrono::Duration::seconds(1),
        "span {span}"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn an_expired_row_reads_as_absent_everywhere_before_it_is_swept() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "expired-agent").await;
    put_task(&server, &agent, "old", json!(1)).await;
    put_task(&server, &agent, "fresh", json!(1)).await;
    put_kv(&server, &agent, "old", json!(1)).await;
    put_kv(&server, &agent, "fresh", json!(1)).await;
    expire_task(&server, &agent, "old").await;
    expire_kv(&server, &agent, "old").await;

    expect_error(get_task(&server, &agent, "old").await, 404, "not_found").await;
    expect_error(delete_task(&server, &agent, "old").await, 404, "not_found").await;
    expect_error(get_kv(&server, &agent, "old").await, 404, "not_found").await;
    expect_error(delete_kv(&server, &agent, "old").await, 404, "not_found").await;
    assert_eq!(
        names(&list(&server, &agent, "/tasks").await, "task_id"),
        ["fresh"]
    );
    assert_eq!(names(&list(&server, &agent, "/kv").await, "key"), ["fresh"]);
    assert_eq!(
        count_rows(&server, "agent_tasks").await,
        2,
        "the sweeper has not run"
    );

    server.cleanup().await;
}

/// The scenario versions exist to rule out: a writer holding version 1 of an entry that has since
/// been deleted and created again must not find its stale `If-Match: 1` matching the new row.
#[tokio::test]
#[serial]
async fn a_stale_if_match_cannot_match_an_entry_deleted_and_created_again() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "aba-agent").await;

    let first: Value = put_task(&server, &agent, "t", json!("first"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(first["version"], 1);
    assert_eq!(delete_task(&server, &agent, "t").await.status(), 204);
    expect_error(get_task(&server, &agent, "t").await, 404, "not_found").await;
    expect_error(delete_task(&server, &agent, "t").await, 404, "not_found").await;

    // Recreated, "only if absent", it continues the version rather than restarting at 1.
    let recreated = put_task_if_match(&server, &agent, "t", "0", json!("second"))
        .send()
        .await
        .unwrap();
    assert_eq!(recreated.status(), 200);
    assert_eq!(recreated.json::<Value>().await.unwrap()["version"], 2);

    // The writer still holding version 1 is refused, and the new row is untouched.
    let stale = put_task_if_match(&server, &agent, "t", "1", json!("stale"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        expect_error(stale, 412, "version_conflict").await["current_version"],
        2
    );
    let read: Value = get_task(&server, &agent, "t").await.json().await.unwrap();
    assert_eq!(read["body"], "second");

    // The same for a key-value entry.
    put_kv(&server, &agent, "k", json!("first")).await;
    assert_eq!(delete_kv(&server, &agent, "k").await.status(), 204);
    let recreated: Value = put_kv(&server, &agent, "k", json!("second"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(recreated["version"], 2);
    let stale = put_kv_if_match(&server, &agent, "k", "1", json!("stale"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        expect_error(stale, 412, "version_conflict").await["current_version"],
        2
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_delete_keeps_the_version_in_a_tombstone_and_nothing_else() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "tombstone-agent").await;
    put_task_in(
        &server,
        &agent,
        "t",
        Some("ctx"),
        "working",
        json!("secret"),
    )
    .await;
    put_task_in(
        &server,
        &agent,
        "t",
        Some("ctx"),
        "completed",
        json!("secret"),
    )
    .await;
    assert_eq!(delete_task(&server, &agent, "t").await.status(), 204);

    let (version, body, context_id, deleted): (i64, Option<String>, Option<String>, bool) =
        sqlx::query_as(
            "SELECT version, body, context_id, deleted_at IS NOT NULL FROM agent_tasks
             WHERE agent_id = $1 AND task_id = 't'",
        )
        .bind(agent.id)
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!((version, body, context_id, deleted), (2, None, None, true));
    assert!(names(&list(&server, &agent, "/tasks").await, "task_id").is_empty());
    assert!(
        names(
            &list(&server, &agent, "/tasks?context_id=ctx").await,
            "task_id"
        )
        .is_empty()
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn an_expired_entry_continues_its_version_before_and_after_the_sweep() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "revive-agent").await;
    for _ in 0..3 {
        for id in ["unswept", "swept"] {
            put_task(&server, &agent, id, json!("old")).await;
            put_kv(&server, &agent, id, json!("old")).await;
        }
    }
    let expire_all = |id: &'static str| {
        let db = server.db.clone();
        async move {
            for table in ["agent_tasks", "agent_kv"] {
                let name = if table == "agent_tasks" {
                    "task_id"
                } else {
                    "key"
                };
                sqlx::query(&format!(
                    "UPDATE {table} SET expires_at = now() - interval '1 hour', \
                     created_at = now() - interval '30 days' WHERE {name} = $1"
                ))
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
            }
        }
    };
    // `swept` is emptied into a tombstone by the sweeper; `unswept` expires only afterwards, so
    // the next write finds it still as it was.
    expire_all("swept").await;
    assert_eq!(sweep(&server.db, TTL_DAYS).await.unwrap(), expired(1, 1));
    expire_all("unswept").await;

    // The row is gone as far as the API is concerned, so a precondition on it sees version 0.
    let phantom = put_task_if_match(&server, &agent, "unswept", "3", json!("x"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        expect_error(phantom, 412, "version_conflict").await["current_version"],
        0
    );

    for id in ["unswept", "swept"] {
        let revived: Value = put_task(&server, &agent, id, json!("new"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(revived["version"], 4, "task {id}");
        let revived: Value = put_kv(&server, &agent, id, json!("new"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(revived["version"], 4, "key {id}");
    }

    // A revived row is a new row: its creation time restarts.
    for table in ["agent_tasks", "agent_kv"] {
        let restarted: bool = sqlx::query_scalar(&format!(
            "SELECT bool_and(created_at > now() - interval '1 minute') FROM {table}"
        ))
        .fetch_one(&server.db)
        .await
        .unwrap();
        assert!(
            restarted,
            "{table}.created_at must restart when an expired row is revived"
        );
    }

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_tombstone_is_kept_for_a_ttl_and_then_forgotten() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "forgetful-agent").await;
    put_task(&server, &agent, "t", json!(1)).await;
    put_kv(&server, &agent, "k", json!(1)).await;
    delete_task(&server, &agent, "t").await;
    delete_kv(&server, &agent, "k").await;

    // A fresh tombstone outlives a sweep: it is what keeps the version.
    assert_eq!(sweep(&server.db, TTL_DAYS).await.unwrap(), Swept::default());
    let kept_for_the_ttl: bool = sqlx::query_scalar(
        "SELECT expires_at > now() + interval '6 days' FROM agent_tasks WHERE task_id = 't'",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert!(
        kept_for_the_ttl,
        "a tombstone is kept for the configured TTL"
    );

    expire_task(&server, &agent, "t").await;
    expire_kv(&server, &agent, "k").await;
    assert_eq!(sweep(&server.db, TTL_DAYS).await.unwrap(), forgotten(1, 1));
    assert_eq!(count_rows(&server, "agent_tasks").await, 0);
    assert_eq!(count_rows(&server, "agent_kv").await, 0);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn rewriting_a_live_row_keeps_its_creation_time() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "keep-created-agent").await;
    put_task(&server, &agent, "t", json!(1)).await;
    put_kv(&server, &agent, "k", json!(1)).await;
    for table in ["agent_tasks", "agent_kv"] {
        sqlx::query(&format!(
            "UPDATE {table} SET created_at = now() - interval '30 days'"
        ))
        .execute(&server.db)
        .await
        .unwrap();
    }
    put_task(&server, &agent, "t", json!(2)).await;
    put_kv(&server, &agent, "k", json!(2)).await;
    for table in ["agent_tasks", "agent_kv"] {
        let kept: bool = sqlx::query_scalar(&format!(
            "SELECT created_at < now() - interval '29 days' FROM {table}"
        ))
        .fetch_one(&server.db)
        .await
        .unwrap();
        assert!(kept, "{table}.created_at must survive a rewrite");
    }
    server.cleanup().await;
}

// ─── The sweeper ────────────────────────────────────────────────────────────

use nasiko_server::agent_state::sweeper::{StoreCounts, Swept, sweep};

/// The TTL the test server runs with, which is also how long the sweeper keeps a tombstone.
const TTL_DAYS: i32 = 7;

/// A pass that removed only expired rows.
fn expired(tasks: u64, kv: u64) -> Swept {
    Swept {
        expired: StoreCounts { tasks, kv },
        ..Swept::default()
    }
}

/// A pass that only forgot tombstones.
fn forgotten(tasks: u64, kv: u64) -> Swept {
    Swept {
        forgotten: StoreCounts { tasks, kv },
        ..Swept::default()
    }
}

/// A pass that removed only rows of deleted agents.
fn orphaned(tasks: u64, kv: u64) -> Swept {
    Swept {
        orphaned: StoreCounts { tasks, kv },
        ..Swept::default()
    }
}

#[tokio::test]
#[serial]
async fn the_sweeper_empties_expired_rows_of_every_agent_and_nothing_live() {
    let server = TestServer::start().await;
    let alice = new_agent(&server, "sweep-alice").await;
    let bob = new_agent(&server, "sweep-bob").await;
    for agent in [&alice, &bob] {
        put_task(&server, agent, "expired", json!(1)).await;
        put_task(&server, agent, "live", json!(1)).await;
        put_kv(&server, agent, "expired", json!(1)).await;
        put_kv(&server, agent, "live", json!(1)).await;
        expire_task(&server, agent, "expired").await;
        expire_kv(&server, agent, "expired").await;
    }

    let swept = sweep(&server.db, TTL_DAYS).await.unwrap();
    assert_eq!(swept, expired(2, 2));

    for agent in [&alice, &bob] {
        assert_eq!(get_task(&server, agent, "live").await.status(), 200);
        assert_eq!(get_kv(&server, agent, "live").await.status(), 200);
    }
    assert_eq!(count_live_rows(&server, "agent_tasks").await, 2);
    assert_eq!(count_live_rows(&server, "agent_kv").await, 2);
    // What expired is kept only as a tombstone: its version, none of its content.
    let emptied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_tasks
         WHERE task_id = 'expired' AND deleted_at IS NOT NULL AND body IS NULL
           AND context_id IS NULL AND version = 1",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert_eq!(emptied, 2);
    assert_eq!(
        sweep(&server.db, TTL_DAYS).await.unwrap(),
        Swept::default(),
        "nothing is left to sweep"
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_sweeper_keeps_going_past_one_batch() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "bulk-agent").await;
    // More than one batch of each (the batch is 1000), plus rows that must survive.
    sqlx::query(
        "INSERT INTO agent_tasks (agent_id, task_id, state, body, version, expires_at)
         SELECT $1, 'expired-' || g, 'working', 'x', 1, now() - interval '1 hour'
         FROM generate_series(1, 2300) g",
    )
    .bind(agent.id)
    .execute(&server.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_kv (agent_id, key, value, version, expires_at)
         SELECT $1, 'expired-' || g, 'x', 1, now() - interval '1 hour'
         FROM generate_series(1, 1200) g",
    )
    .bind(agent.id)
    .execute(&server.db)
    .await
    .unwrap();
    put_task(&server, &agent, "live", json!(1)).await;
    put_kv(&server, &agent, "live", json!(1)).await;

    let swept = sweep(&server.db, TTL_DAYS).await.unwrap();
    assert_eq!(swept, expired(2300, 1200));
    assert_eq!(count_live_rows(&server, "agent_tasks").await, 1);
    assert_eq!(count_live_rows(&server, "agent_kv").await, 1);

    // Once those tombstones' own expiry passes, they are forgotten, again past one batch.
    for table in ["agent_tasks", "agent_kv"] {
        sqlx::query(&format!(
            "UPDATE {table} SET expires_at = now() - interval '1 second' WHERE deleted_at IS NOT NULL"
        ))
        .execute(&server.db)
        .await
        .unwrap();
    }
    assert_eq!(
        sweep(&server.db, TTL_DAYS).await.unwrap(),
        forgotten(2300, 1200)
    );
    assert_eq!(count_rows(&server, "agent_tasks").await, 1);
    assert_eq!(count_rows(&server, "agent_kv").await, 1);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_sweeper_leaves_rows_a_writer_holds_instead_of_waiting_for_them() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "locked-agent").await;
    put_task(&server, &agent, "held", json!(1)).await;
    put_task(&server, &agent, "free", json!(1)).await;
    expire_task(&server, &agent, "held").await;
    expire_task(&server, &agent, "free").await;

    // A writer in the middle of its transaction holds the row it is about to rewrite.
    let peer = peer_pool(&server).await;
    let mut writer = peer.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM agent_tasks WHERE task_id = 'held' FOR UPDATE")
        .execute(&mut *writer)
        .await
        .unwrap();

    // The pass must come back, not queue behind the writer, and take only what is free.
    let swept = tokio::time::timeout(Duration::from_secs(10), sweep(&server.db, TTL_DAYS))
        .await
        .expect("the sweeper must not wait on a row another transaction holds")
        .unwrap();
    assert_eq!(swept, expired(1, 0));
    assert_eq!(count_live_rows(&server, "agent_tasks").await, 1);

    // Once the writer lets go, the next pass collects it.
    writer.rollback().await.unwrap();
    assert_eq!(sweep(&server.db, TTL_DAYS).await.unwrap(), expired(1, 0));
    assert_eq!(count_live_rows(&server, "agent_tasks").await, 0);

    peer.close().await;
    server.cleanup().await;
}

// ─── Writers of one entry take turns ────────────────────────────────────────

/// The advisory-lock class `repo.rs` takes entry locks under. Mirrored here because the test has
/// to hold the same lock a concurrent writer would; if the two drift, the tests below fail at
/// their first assertion (nothing waits), not silently.
const ENTRY_LOCK_CLASS: i32 = 0x0A9E_517A;

/// Blocks until some backend is waiting on an advisory lock in this test's database.
async fn wait_for_a_blocked_writer(peer: &PgPool) {
    for _ in 0..100 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks
             WHERE locktype = 'advisory' AND NOT granted
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .fetch_one(peer)
        .await
        .unwrap();
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no writer ever waited on the entry lock: the write did not take it");
}

/// Holds `lock_key`'s entry lock the way a writer mid-transaction would, starts `request`, and
/// checks that the request waits for the lock and then completes once it is released.
///
/// Firing two writes at once cannot show this: each is a short transaction and the first commits
/// before the second looks, so the test would pass with or without the lock.
async fn assert_write_waits_for_the_entry_lock(
    server: &TestServer,
    lock_key: String,
    request: RequestBuilder,
) {
    let peer = peer_pool(server).await;
    let mut holder = peer.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(ENTRY_LOCK_CLASS)
        .bind(&lock_key)
        .execute(&mut *holder)
        .await
        .unwrap();

    let write = tokio::spawn(async move { request.send().await.unwrap() });
    wait_for_a_blocked_writer(&peer).await;
    assert!(
        !write.is_finished(),
        "the write must wait while the entry is locked"
    );

    holder.rollback().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(10), write)
        .await
        .expect("the write must proceed once the lock is released")
        .unwrap();
    assert_eq!(res.status(), 200);

    peer.close().await;
}

#[tokio::test]
#[serial]
async fn a_task_write_waits_for_a_concurrent_writer_of_the_same_task() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "lock-task-agent").await;
    let request = call(&server, &agent, Method::PUT, "/tasks/contended")
        .json(&json!({"context_id": null, "state": "working", "body": 1}));
    let lock_key = format!("{}/task/contended", agent.id);
    assert_write_waits_for_the_entry_lock(&server, lock_key, request).await;

    // It then wrote at version 1: the lock holder never wrote anything.
    let read: Value = get_task(&server, &agent, "contended")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read["version"], 1);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_kv_write_waits_for_a_concurrent_writer_of_the_same_key() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "lock-kv-agent").await;
    let request = call(&server, &agent, Method::PUT, "/kv/contended").json(&json!({"value": 1}));
    let lock_key = format!("{}/kv/contended", agent.id);
    assert_write_waits_for_the_entry_lock(&server, lock_key, request).await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_delete_waits_for_a_concurrent_writer_of_the_same_entry() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "lock-delete-agent").await;
    put_task(&server, &agent, "contended", json!(1)).await;
    let peer = peer_pool(&server).await;
    let mut holder = peer.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(ENTRY_LOCK_CLASS)
        .bind(format!("{}/task/contended", agent.id))
        .execute(&mut *holder)
        .await
        .unwrap();

    let request = call(&server, &agent, Method::DELETE, "/tasks/contended");
    let delete = tokio::spawn(async move { request.send().await.unwrap() });
    wait_for_a_blocked_writer(&peer).await;
    assert!(
        !delete.is_finished(),
        "the delete must wait while the entry is locked"
    );

    holder.rollback().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(10), delete)
        .await
        .expect("the delete must proceed once the lock is released")
        .unwrap();
    assert_eq!(res.status(), 204);

    peer.close().await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_write_that_waited_for_the_lock_continues_the_version_and_is_stamped_after_it() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "lock-order-agent").await;
    assert_a_waiting_write_continues_the_version(&server, &agent).await;
    server.cleanup().await;
}

/// The same, on a database whose sessions default to SERIALIZABLE: the write flow states its own
/// isolation, so a default it did not choose cannot turn the wait into a serialization failure.
#[tokio::test]
#[serial]
async fn a_waiting_write_continues_the_version_whatever_the_default_isolation() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "serializable-agent").await;
    sqlx::query(
        "DO $$ BEGIN EXECUTE format(
             'ALTER DATABASE %I SET default_transaction_isolation = serializable',
             current_database());
         END $$",
    )
    .execute(&server.db)
    .await
    .unwrap();
    // Sessions read the default when they connect, so end the server's open one; its pool
    // reconnects on the next request, under the new default.
    let peer = peer_pool(&server).await;
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
         WHERE datname = current_database() AND pid <> pg_backend_pid()",
    )
    .execute(&peer)
    .await
    .unwrap();
    peer.close().await;
    let isolation: String = sqlx::query_scalar("SHOW default_transaction_isolation")
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(isolation, "serializable");

    assert_a_waiting_write_continues_the_version(&server, &agent).await;
    server.cleanup().await;
}

/// A first writer holds an entry's lock and commits version 1 while a second write, already
/// started, waits; the second must then write version 2, stamped after the first.
async fn assert_a_waiting_write_continues_the_version(server: &TestServer, agent: &Agent) {
    let peer = peer_pool(server).await;

    // A first writer holds the entry's lock, and the second begins while it does.
    let mut first = peer.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(ENTRY_LOCK_CLASS)
        .bind(format!("{}/task/ordered", agent.id))
        .execute(&mut *first)
        .await
        .unwrap();
    let request = call(server, agent, Method::PUT, "/tasks/ordered")
        .json(&json!({"context_id": null, "state": "working", "body": "second"}));
    let second = tokio::spawn(async move { request.send().await.unwrap() });
    wait_for_a_blocked_writer(&peer).await;

    // The first writer commits its version 1 while the second is already in flight.
    let first_stamped_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO agent_tasks (agent_id, task_id, state, body, version, updated_at, expires_at)
         VALUES ($1, 'ordered', 'working', 'placeholder', 1, clock_timestamp(),
                 clock_timestamp() + interval '7 days')
         RETURNING updated_at",
    )
    .bind(agent.id)
    .fetch_one(&mut *first)
    .await
    .unwrap();
    first.commit().await.unwrap();

    // The second one saw it: it is version 2, and stamped after the write it waited for, not at
    // the moment its transaction began (which was before that write existed).
    let res = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("the second write must proceed once the first commits")
        .unwrap();
    assert_eq!(res.status(), 200);
    let stamp: Value = res.json().await.unwrap();
    assert_eq!(stamp["version"], 2);
    assert!(
        timestamp(&stamp, "updated_at") > first_stamped_at,
        "a later version must not be stamped earlier than the one before it"
    );

    peer.close().await;
}

// ─── When an agent goes away ────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn removing_an_agent_row_removes_its_state_and_only_its_state() {
    let server = TestServer::start().await;
    let doomed = new_agent(&server, "doomed-agent").await;
    let bystander = new_agent(&server, "bystander-agent").await;
    for agent in [&doomed, &bystander] {
        put_task(&server, agent, "t", json!(1)).await;
        put_kv(&server, agent, "k", json!(1)).await;
    }
    assert_eq!(count_rows(&server, "agent_tasks").await, 2);
    assert_eq!(count_rows(&server, "agent_kv").await, 2);

    // A hard delete, as `agents::utils::delete_agent_or_mark_failed` performs for a build that
    // never succeeded: the foreign keys do the cleanup, there is no explicit delete to forget.
    sqlx::query("DELETE FROM agents WHERE id = $1")
        .bind(doomed.id)
        .execute(&server.db)
        .await
        .unwrap();

    assert_eq!(count_rows(&server, "agent_tasks").await, 1);
    assert_eq!(count_rows(&server, "agent_kv").await, 1);
    assert_eq!(get_task(&server, &bystander, "t").await.status(), 200);
    assert_eq!(get_kv(&server, &bystander, "k").await.status(), 200);

    server.cleanup().await;
}

/// `DELETE /api/agents/{id}` soft-deletes: the row stays, so the foreign keys never fire. The
/// credential stops working at once, and the sweeper's next pass removes the state, live or not.
#[tokio::test]
#[serial]
async fn a_soft_deleted_agent_loses_access_to_its_state_and_the_sweeper_removes_it() {
    let server = TestServer::start().await;
    let admin: Value = server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin_id: Uuid = admin["user_id"].as_str().unwrap().parse().unwrap();
    let id = seed_agent_row(&server, admin_id, "soft-deleted-agent").await;
    let agent = Agent {
        id,
        token: mint_state_token(&server, id).await,
    };
    let bystander = new_agent(&server, "bystander-of-delete").await;
    for owner in [&agent, &bystander] {
        put_task(&server, owner, "t", json!(1)).await;
        put_kv(&server, owner, "k", json!(1)).await;
    }

    let deleted = common::as_superuser(
        server
            .client
            .delete(server.url(&format!("/api/agents/{id}"))),
        &admin_id.to_string(),
        "admin",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(deleted.status(), 200);

    expect_error(get_task(&server, &agent, "t").await, 401, "unauthorized").await;
    expect_error(get_kv(&server, &agent, "k").await, 401, "unauthorized").await;
    expect_error(
        put_task(&server, &agent, "t", json!(2)).await,
        401,
        "unauthorized",
    )
    .await;

    // Nothing has expired, yet the deleted agent's rows go, and only its rows.
    assert_eq!(sweep(&server.db, TTL_DAYS).await.unwrap(), orphaned(1, 1));
    assert_eq!(count_rows(&server, "agent_tasks").await, 1);
    assert_eq!(count_rows(&server, "agent_kv").await, 1);
    assert_eq!(get_task(&server, &bystander, "t").await.status(), 200);
    assert_eq!(sweep(&server.db, TTL_DAYS).await.unwrap(), Swept::default());

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_sweeper_removes_a_deleted_agents_rows_past_one_batch() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "bulk-deleted-agent").await;
    sqlx::query(
        "INSERT INTO agent_tasks (agent_id, task_id, state, body, version, expires_at)
         SELECT $1, 'task-' || g, 'working', 'x', 1, now() + interval '1 day'
         FROM generate_series(1, 1500) g",
    )
    .bind(agent.id)
    .execute(&server.db)
    .await
    .unwrap();
    sqlx::query("UPDATE agents SET deleted_at = now() WHERE id = $1")
        .bind(agent.id)
        .execute(&server.db)
        .await
        .unwrap();

    assert_eq!(
        sweep(&server.db, TTL_DAYS).await.unwrap(),
        orphaned(1500, 0)
    );
    assert_eq!(count_rows(&server, "agent_tasks").await, 0);
    server.cleanup().await;
}

// ─── Registration ───────────────────────────────────────────────────────────

async fn register(server: &TestServer, agent: &Agent, body: Value) -> Response {
    call(server, agent, Method::POST, "/register")
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn state_mode(server: &TestServer, agent: &Agent) -> String {
    sqlx::query_scalar("SELECT state_mode FROM agents WHERE id = $1")
        .bind(agent.id)
        .fetch_one(&server.db)
        .await
        .unwrap()
}

#[tokio::test]
#[serial]
async fn an_agent_starts_out_keeping_its_state_in_memory() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "fresh-agent").await;
    assert_eq!(state_mode(&server, &agent).await, "memory");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn registering_a_task_store_marks_the_agent_durable() {
    let server = TestServer::start().await;
    let tasks_only = new_agent(&server, "tasks-agent").await;
    let both = new_agent(&server, "both-agent").await;
    let bystander = new_agent(&server, "other-agent").await;

    let res = register(
        &server,
        &tasks_only,
        json!({"capabilities": ["tasks"], "client": "nasiko-sdk/1.2.3"}),
    )
    .await;
    assert_eq!(res.status(), 204);
    assert!(res.text().await.unwrap().is_empty(), "204 carries no body");
    assert_eq!(state_mode(&server, &tasks_only).await, "durable");

    let res = register(
        &server,
        &both,
        json!({"capabilities": ["tasks", "kv"], "client": "sdk"}),
    )
    .await;
    assert_eq!(res.status(), 204);
    assert_eq!(state_mode(&server, &both).await, "durable");

    assert_eq!(
        state_mode(&server, &bystander).await,
        "memory",
        "registration is per agent"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn registering_only_a_kv_store_leaves_the_agent_in_memory_mode() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "kv-only-agent").await;
    let res = register(
        &server,
        &agent,
        json!({"capabilities": ["kv"], "client": "sdk"}),
    )
    .await;
    assert_eq!(res.status(), 204);
    assert_eq!(state_mode(&server, &agent).await, "memory");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn registration_is_idempotent_and_never_steps_an_agent_back_down() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "repeat-agent").await;
    assert_eq!(
        register(&server, &agent, json!({"capabilities": ["tasks"]}))
            .await
            .status(),
        204
    );
    let updated_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT updated_at FROM agents WHERE id = $1")
            .bind(agent.id)
            .fetch_one(&server.db)
            .await
            .unwrap();

    // An SDK registers at every start: doing it again must neither fail nor touch the agent row.
    assert_eq!(
        register(&server, &agent, json!({"capabilities": ["tasks"]}))
            .await
            .status(),
        204
    );
    let after: DateTime<Utc> = sqlx::query_scalar("SELECT updated_at FROM agents WHERE id = $1")
        .bind(agent.id)
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(
        after, updated_at,
        "an already-durable agent's row must not be rewritten"
    );

    // A later KV-only registration does not undo it.
    assert_eq!(
        register(&server, &agent, json!({"capabilities": ["kv"]}))
            .await
            .status(),
        204
    );
    assert_eq!(state_mode(&server, &agent).await, "durable");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_malformed_registration_is_refused_and_changes_nothing() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "malformed-agent").await;
    for bad in [
        json!({"capabilities": []}),
        json!({"capabilities": ["blobs"]}),
        json!({"capabilities": ["tasks", "blobs"]}),
        json!({"capabilities": "tasks"}),
        json!({"client": "sdk"}),
        json!({"capabilities": ["tasks"], "extra": true}),
        json!({"capabilities": ["tasks"], "client": ""}),
        json!({"capabilities": ["tasks"], "client": "x".repeat(129)}),
        json!({"capabilities": ["tasks"], "client": "line\nbreak"}),
    ] {
        let res = register(&server, &agent, bad.clone()).await;
        expect_error(res, 400, "invalid_request").await;
    }
    assert_eq!(state_mode(&server, &agent).await, "memory");
    server.cleanup().await;
}

// ─── Encryption at rest ─────────────────────────────────────────────────────

/// The raw stored column of one row.
async fn stored_column(server: &TestServer, agent: &Agent, store: &str, name: &str) -> String {
    let (table, column, key) = match store {
        "tasks" => ("agent_tasks", "body", "task_id"),
        _ => ("agent_kv", "value", "key"),
    };
    sqlx::query_scalar(&format!(
        "SELECT {column} FROM {table} WHERE agent_id = $1 AND {key} = $2"
    ))
    .bind(agent.id)
    .bind(name)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

async fn overwrite_column(
    server: &TestServer,
    agent: &Agent,
    store: &str,
    name: &str,
    sealed: &str,
) {
    let (table, column, key) = match store {
        "tasks" => ("agent_tasks", "body", "task_id"),
        _ => ("agent_kv", "value", "key"),
    };
    sqlx::query(&format!(
        "UPDATE {table} SET {column} = $3 WHERE agent_id = $1 AND {key} = $2"
    ))
    .bind(agent.id)
    .bind(name)
    .bind(sealed)
    .execute(&server.db)
    .await
    .unwrap();
}

#[tokio::test]
#[serial]
async fn task_bodies_and_values_are_stored_as_ciphertext_under_a_key_of_their_own() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "sealed-agent").await;
    put_task(
        &server,
        &agent,
        "t",
        json!({"secret": "PLAINTEXT-MARKER-4821"}),
    )
    .await;
    put_kv(&server, &agent, "k", json!("PLAINTEXT-MARKER-9034")).await;

    for (store, name) in [("tasks", "t"), ("kv", "k")] {
        let stored = stored_column(&server, &agent, store, name).await;
        assert!(
            !stored.contains("PLAINTEXT-MARKER"),
            "the {store} secret is in the database in the clear"
        );
        assert!(
            serde_json::from_str::<Value>(&stored).is_err(),
            "the {store} column holds JSON, not ciphertext"
        );
        // Not under the key that protects the agent's secrets either: a separate domain.
        assert!(
            nasiko_secrets::SecretsCrypto::for_agent(agent.id)
                .decrypt(&stored)
                .is_err()
        );
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_ciphertext_moved_onto_another_row_does_not_open_there() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "moved-agent").await;
    let other = new_agent(&server, "moved-other-agent").await;
    put_task(&server, &agent, "a", json!("from a")).await;
    put_task(&server, &agent, "b", json!("from b")).await;
    put_kv(&server, &agent, "a", json!("kv a")).await;
    put_task(&server, &other, "a", json!("other a")).await;
    let sealed_a = stored_column(&server, &agent, "tasks", "a").await;

    // Another task of the same agent: same key, different row.
    let sealed_b = stored_column(&server, &agent, "tasks", "b").await;
    overwrite_column(&server, &agent, "tasks", "b", &sealed_a).await;
    expect_error(get_task(&server, &agent, "b").await, 500, "internal_error").await;
    // Put back where it belongs, it opens again: the row, not the bytes, was wrong.
    overwrite_column(&server, &agent, "tasks", "b", &sealed_b).await;
    let read: Value = get_task(&server, &agent, "b").await.json().await.unwrap();
    assert_eq!(read["body"], "from b");

    // The same name in the other store.
    overwrite_column(&server, &agent, "kv", "a", &sealed_a).await;
    expect_error(get_kv(&server, &agent, "a").await, 500, "internal_error").await;

    // The same task id under another agent.
    overwrite_column(&server, &other, "tasks", "a", &sealed_a).await;
    expect_error(get_task(&server, &other, "a").await, 500, "internal_error").await;

    server.cleanup().await;
}

// ─── Indexes ────────────────────────────────────────────────────────────────

/// The plan Postgres picks for `query` with sorts and every non-index scan priced out, so that an
/// empty test table still shows whether an index can serve the query and its order at all: if
/// none can, the plan sorts anyway.
async fn plan_of(server: &TestServer, query: &str) -> String {
    let mut tx = server.db.begin().await.unwrap();
    for setting in ["enable_seqscan", "enable_bitmapscan", "enable_sort"] {
        sqlx::query(&format!("SET LOCAL {setting} = off"))
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let lines: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {query}"))
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    lines.join("\n")
}

#[tokio::test]
#[serial]
async fn names_compare_as_bytes_whatever_the_database_locale() {
    let server = TestServer::start().await;
    for (table, column) in [("agent_tasks", "task_id"), ("agent_kv", "key")] {
        let collation: Option<String> = sqlx::query_scalar(
            "SELECT collation_name FROM information_schema.columns
             WHERE table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_one(&server.db)
        .await
        .unwrap();
        assert_eq!(collation.as_deref(), Some("C"), "{table}.{column}");
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_task_listing_is_served_in_order_by_its_index() {
    let server = TestServer::start().await;
    // The listing's shape: one agent's live rows, newest first, resuming from a cursor.
    let plan = plan_of(
        &server,
        "SELECT task_id FROM agent_tasks
         WHERE agent_id = '00000000-0000-0000-0000-000000000001'
           AND deleted_at IS NULL AND expires_at > now()
           AND (updated_at < now() OR (updated_at = now() AND task_id > 'a'))
         ORDER BY updated_at DESC, task_id ASC
         LIMIT 51",
    )
    .await;
    assert!(plan.contains("idx_agent_tasks_listing"), "{plan}");
    assert!(
        !plan.contains("Sort"),
        "the index must give the order: {plan}"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_kv_listing_is_served_in_order_by_the_primary_key_prefix_included() {
    let server = TestServer::start().await;
    let plan = plan_of(
        &server,
        "SELECT key FROM agent_kv
         WHERE agent_id = '00000000-0000-0000-0000-000000000001'
           AND deleted_at IS NULL AND expires_at > now()
           AND starts_with(key, 'app/') AND key > 'app/a'
         ORDER BY key ASC
         LIMIT 51",
    )
    .await;
    assert!(plan.contains("agent_kv_pkey"), "{plan}");
    assert!(
        !plan.contains("Sort"),
        "the key must give the order: {plan}"
    );
    // Under the C collation the prefix becomes a range on the key rather than a filter.
    assert!(plan.contains("key >= 'app/'"), "{plan}");
    server.cleanup().await;
}

// ─── What the API refuses to accept ─────────────────────────────────────────

#[tokio::test]
#[serial]
async fn a_name_the_store_cannot_hold_is_a_bad_request_never_a_server_error() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "names-agent").await;

    // NUL cannot be stored in Postgres text, so it would be a 500 if it got that far.
    for bad in [
        "a\0b",
        "line\nbreak",
        "tab\there",
        "bell\u{7}",
        &"k".repeat(513),
        &"é".repeat(257),
    ] {
        expect_error(
            put_kv(&server, &agent, bad, json!(1)).await,
            400,
            "invalid_request",
        )
        .await;
        expect_error(get_kv(&server, &agent, bad).await, 400, "invalid_request").await;
        expect_error(
            delete_kv(&server, &agent, bad).await,
            400,
            "invalid_request",
        )
        .await;
    }
    for bad in ["a\0b", "line\nbreak", &"t".repeat(257), &"é".repeat(129)] {
        expect_error(
            put_task(&server, &agent, bad, json!(1)).await,
            400,
            "invalid_request",
        )
        .await;
        expect_error(get_task(&server, &agent, bad).await, 400, "invalid_request").await;
        expect_error(
            delete_task(&server, &agent, bad).await,
            400,
            "invalid_request",
        )
        .await;
    }

    // The longest names allowed are accepted.
    assert_eq!(
        put_kv(&server, &agent, &"k".repeat(512), json!(1))
            .await
            .status(),
        200
    );
    assert_eq!(
        put_kv(&server, &agent, &"é".repeat(256), json!(1))
            .await
            .status(),
        200
    );
    assert_eq!(
        put_task(&server, &agent, &"t".repeat(256), json!(1))
            .await
            .status(),
        200
    );
    assert_eq!(
        put_task(&server, &agent, &"é".repeat(128), json!(1))
            .await
            .status(),
        200
    );
    assert_eq!(count_rows(&server, "agent_kv").await, 2);
    assert_eq!(count_rows(&server, "agent_tasks").await, 2);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_body_the_api_does_not_understand_is_a_bad_request() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "bodies-agent").await;
    let long_context = "c".repeat(257);
    let long_state = "s".repeat(129);

    let task_bodies = [
        json!({"state": "working", "body": 1, "extra": true}),
        json!({"body": 1}),
        json!({"state": "working"}),
        json!({"state": "", "body": 1}),
        json!({"state": long_state, "body": 1}),
        json!({"state": "line\nbreak", "body": 1}),
        json!({"context_id": "", "state": "working", "body": 1}),
        json!({"context_id": long_context, "state": "working", "body": 1}),
        json!({"context_id": "a\0b", "state": "working", "body": 1}),
        json!({"context_id": 7, "state": "working", "body": 1}),
        json!({"state": 7, "body": 1}),
    ];
    for bad in task_bodies {
        let res = call(&server, &agent, Method::PUT, "/tasks/t")
            .json(&bad)
            .send()
            .await
            .unwrap();
        expect_error(res, 400, "invalid_request").await;
    }
    for bad in [
        json!({}),
        json!({"value": 1, "extra": 1}),
        json!({"val": 1}),
    ] {
        let res = call(&server, &agent, Method::PUT, "/kv/k")
            .json(&bad)
            .send()
            .await
            .unwrap();
        expect_error(res, 400, "invalid_request").await;
    }
    assert_eq!(count_rows(&server, "agent_tasks").await, 0);
    assert_eq!(count_rows(&server, "agent_kv").await, 0);

    // Not JSON at all, no body, and the wrong content type are the same kind of mistake.
    let garbage = call(&server, &agent, Method::PUT, "/kv/k")
        .header("Content-Type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    expect_error(garbage, 400, "invalid_request").await;
    let empty = call(&server, &agent, Method::PUT, "/kv/k")
        .header("Content-Type", "application/json")
        .send()
        .await
        .unwrap();
    expect_error(empty, 400, "invalid_request").await;
    let wrong_type = call(&server, &agent, Method::PUT, "/kv/k")
        .header("Content-Type", "text/plain")
        .body("{\"value\": 1}")
        .send()
        .await
        .unwrap();
    expect_error(wrong_type, 400, "invalid_request").await;

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn listing_parameters_outside_the_contract_are_a_bad_request() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "params-agent").await;
    for path in [
        "/tasks?limit=0",
        "/tasks?limit=101",
        "/tasks?limit=-5",
        "/tasks?limit=many",
        "/tasks?bogus=1",
        "/tasks?context_id=a%00b",
        "/tasks?state=a%0Ab",
        "/kv?limit=0",
        "/kv?limit=101",
        "/kv?order=desc",
        "/kv?prefix=a%00b",
    ] {
        let res = call(&server, &agent, Method::GET, path)
            .send()
            .await
            .unwrap();
        expect_error(res, 400, "invalid_request").await;
    }
    for path in [
        "/tasks?cursor=garbage",
        "/kv?cursor=garbage",
        "/tasks?cursor=e30",
        "/kv?cursor=e30",
    ] {
        let res = call(&server, &agent, Method::GET, path)
            .send()
            .await
            .unwrap();
        expect_error(res, 400, "invalid_cursor").await;
    }
    // The extremes of the allowed range are fine.
    for path in [
        "/tasks?limit=1",
        "/tasks?limit=100",
        "/kv?limit=1",
        "/kv?limit=100",
    ] {
        assert_eq!(list(&server, &agent, path).await["items"], json!([]));
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_query_value_left_empty_is_the_same_as_leaving_it_out() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "empty-params-agent").await;
    put_task_in(&server, &agent, "t", Some("ctx"), "working", json!(1)).await;
    put_kv(&server, &agent, "k", json!(1)).await;
    let tasks = list(&server, &agent, "/tasks?context_id=&state=&limit=&cursor=").await;
    assert_eq!(names(&tasks, "task_id"), ["t"]);
    let entries = list(&server, &agent, "/kv?prefix=&limit=&cursor=").await;
    assert_eq!(names(&entries, "key"), ["k"]);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_cursor_from_one_agents_listing_cannot_widen_another_agents_view() {
    let server = TestServer::start().await;
    let alice = new_agent(&server, "cursor-alice").await;
    let bob = new_agent(&server, "cursor-bob").await;
    for id in ["a1", "a2", "a3"] {
        put_task(&server, &alice, id, json!("alice")).await;
    }
    put_task(&server, &bob, "b1", json!("bob")).await;
    let page = list(&server, &alice, "/tasks?limit=1").await;
    let alices_cursor = page["next_cursor"].as_str().unwrap().to_string();

    // The cursor is only a position. Bob still sees nothing but his own rows.
    let bobs = list(&server, &bob, &format!("/tasks?cursor={alices_cursor}")).await;
    for item in bobs["items"].as_array().unwrap() {
        assert_eq!(item["body"], "bob");
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_two_listings_do_not_accept_each_others_cursors() {
    let server = TestServer::start().await;
    let agent = new_agent(&server, "cross-cursor-agent").await;
    for id in ["a", "b"] {
        put_task(&server, &agent, id, json!(1)).await;
        put_kv(&server, &agent, id, json!(1)).await;
    }
    let task_cursor = list(&server, &agent, "/tasks?limit=1").await["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let kv_cursor = list(&server, &agent, "/kv?limit=1").await["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let res = call(
        &server,
        &agent,
        Method::GET,
        &format!("/kv?cursor={task_cursor}"),
    )
    .send()
    .await
    .unwrap();
    expect_error(res, 400, "invalid_cursor").await;
    let res = call(
        &server,
        &agent,
        Method::GET,
        &format!("/tasks?cursor={kv_cursor}"),
    )
    .send()
    .await
    .unwrap();
    expect_error(res, 400, "invalid_cursor").await;
    server.cleanup().await;
}
