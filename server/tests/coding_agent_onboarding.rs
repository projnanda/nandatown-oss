//! `/api/me/coding-agents/sync*`: the sync progress local clients report.

mod common;

use reqwest::StatusCode;
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

async fn create_user(server: &common::TestServer, username: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, is_superuser) VALUES ($1, $2, false) RETURNING id",
    )
    .bind(username)
    .bind(format!("{username}@test.local"))
    .fetch_one(&server.db)
    .await
    .unwrap()
}

fn as_user(rb: reqwest::RequestBuilder, user: Uuid) -> reqwest::RequestBuilder {
    common::as_member(rb, &user.to_string(), "member")
}

async fn get(server: &common::TestServer, user: Uuid, path: &str) -> (StatusCode, Value) {
    let res = as_user(server.client.get(server.url(path)), user)
        .send()
        .await
        .unwrap();
    let status = res.status();
    (status, res.json().await.unwrap_or(Value::Null))
}

/// PUT a JSON body; returns the status and the raw text (plain text on 400, JSON on 200).
async fn put(
    server: &common::TestServer,
    user: Uuid,
    path: &str,
    body: &Value,
) -> (StatusCode, String) {
    let res = as_user(server.client.put(server.url(path)), user)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = res.status();
    (status, res.text().await.unwrap())
}

fn sync_body(phase: &str) -> Value {
    json!({
        "device_name": "Example MacBook",
        "client": "desktop",
        "phase": phase,
        "agents": [{
            "id": "claude",
            "name": "Claude Code",
            "detected": true,
            "connected": true,
            "import": {
                "status": "running", "done": 40, "total": 123,
                "sessions": 98, "turns": 753, "failed": 0, "error": null
            }
        }],
        "error": null
    })
}

/// A v1 body as the server returns it: every v2 field present with its default.
fn with_v2_defaults(mut body: Value) -> Value {
    for agent in body["agents"].as_array_mut().unwrap() {
        let import = &mut agent["import"];
        import["pending"] = json!(0);
        import["rejected"] = json!(0);
        import["rejected_reasons"] = json!([]);
    }
    body
}

async fn put_sync(server: &common::TestServer, user: Uuid, device: &str, phase: &str) -> Value {
    let (status, text) = put(
        server,
        user,
        &format!("/api/me/coding-agents/sync/{device}"),
        &sync_body(phase),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
#[serial]
async fn coding_agent_routes_require_auth() {
    let server = common::TestServer::start().await;

    let res = server
        .client
        .get(server.url("/api/me/coding-agents/sync"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let res = server
        .client
        .put(server.url("/api/me/coding-agents/sync/dev1"))
        .json(&sync_body("discovering"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn sync_report_round_trips_newest_first() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "sync-user").await;

    let (_, empty) = get(&server, user, "/api/me/coding-agents/sync").await;
    assert_eq!(empty, json!({"runs": []}));

    let stored = put_sync(&server, user, "laptop_1", "importing").await;
    assert_eq!(stored["device_id"], "laptop_1");
    assert_eq!(stored["device_name"], "Example MacBook");
    assert_eq!(stored["client"], "desktop");
    assert_eq!(stored["phase"], "importing");
    assert_eq!(
        stored["agents"],
        with_v2_defaults(sync_body("importing"))["agents"]
    );
    assert_eq!(stored["error"], Value::Null);
    assert!(stored["updated_at"].is_string());

    put_sync(&server, user, "desktop-2", "done").await;

    let (status, body) = get(&server, user, "/api/me/coding-agents/sync").await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().unwrap();
    let devices: Vec<&str> = runs
        .iter()
        .map(|r| r["device_id"].as_str().unwrap())
        .collect();
    assert_eq!(devices, ["desktop-2", "laptop_1"]);
    assert_eq!(runs[1], stored);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn sync_report_validation_failures_are_plain_text_400s() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "sync-bad").await;

    let bad_device = put(
        &server,
        user,
        "/api/me/coding-agents/sync/bad.device",
        &sync_body("discovering"),
    )
    .await;
    let long_device = put(
        &server,
        user,
        &format!("/api/me/coding-agents/sync/{}", "a".repeat(65)),
        &sync_body("discovering"),
    )
    .await;

    let mut too_many = sync_body("importing");
    let agent = too_many["agents"][0].clone();
    too_many["agents"] = Value::Array(vec![agent; 11]);
    let too_many = put(&server, user, "/api/me/coding-agents/sync/dev1", &too_many).await;

    let bad_phase = put(
        &server,
        user,
        "/api/me/coding-agents/sync/dev1",
        &sync_body("paused"),
    )
    .await;

    let mut bad_status = sync_body("importing");
    bad_status["agents"][0]["import"]["status"] = json!("queued");
    let bad_status = put(
        &server,
        user,
        "/api/me/coding-agents/sync/dev1",
        &bad_status,
    )
    .await;

    for (case, (status, text)) in [
        ("bad device_id", bad_device),
        ("long device_id", long_device),
        ("11 agents", too_many),
        ("bad phase", bad_phase),
        ("bad import status", bad_status),
    ] {
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {text}");
        assert!(!text.is_empty(), "{case}: 400 must say why");
    }

    let (_, body) = get(&server, user, "/api/me/coding-agents/sync").await;
    assert_eq!(body, json!({"runs": []}), "nothing invalid was stored");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn users_see_only_their_own_runs() {
    let server = common::TestServer::start().await;
    let alice = create_user(&server, "alice").await;
    let bob = create_user(&server, "bob").await;

    // Same device id on both accounts: rows are keyed by (user, device), so neither overwrites.
    put_sync(&server, alice, "shared-device", "importing").await;
    put_sync(&server, bob, "shared-device", "awaiting_consent").await;

    let (_, alice_runs) = get(&server, alice, "/api/me/coding-agents/sync").await;
    let (_, bob_runs) = get(&server, bob, "/api/me/coding-agents/sync").await;
    assert_eq!(alice_runs["runs"].as_array().unwrap().len(), 1);
    assert_eq!(alice_runs["runs"][0]["phase"], "importing");
    assert_eq!(bob_runs["runs"].as_array().unwrap().len(), 1);
    assert_eq!(bob_runs["runs"][0]["phase"], "awaiting_consent");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn runs_older_than_a_day_are_not_listed() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "stale-user").await;

    put_sync(&server, user, "old-device", "done").await;
    put_sync(&server, user, "new-device", "done").await;
    sqlx::query(
        "UPDATE coding_agent_sync_runs SET updated_at = now() - interval '25 hours' \
          WHERE user_id = $1 AND device_id = 'old-device'",
    )
    .bind(user)
    .execute(&server.db)
    .await
    .unwrap();

    let (_, body) = get(&server, user, "/api/me/coding-agents/sync").await;
    let runs = body["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["device_id"], "new-device");

    server.cleanup().await;
}

async fn started_at(server: &common::TestServer, user: Uuid, phase: &str) -> Value {
    put_sync(server, user, "dev", phase).await["started_at"].clone()
}

/// Backdate the stored start so a restamp is distinguishable from a kept value.
async fn backdate_start(server: &common::TestServer, user: Uuid) {
    sqlx::query(
        "UPDATE coding_agent_sync_runs SET started_at = started_at - interval '1 hour' \
          WHERE user_id = $1 AND device_id = 'dev'",
    )
    .bind(user)
    .execute(&server.db)
    .await
    .unwrap();
}

#[tokio::test]
#[serial]
async fn started_at_stamps_when_a_run_enters_discovering() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "started-user").await;

    // A first row that is not `discovering` has no start yet.
    assert_eq!(
        started_at(&server, user, "awaiting_consent").await,
        Value::Null
    );

    // awaiting_consent → discovering opens a run.
    let first = started_at(&server, user, "discovering").await;
    assert!(first.is_string());

    // Moving through the run keeps the start, including a repeated `discovering`.
    backdate_start(&server, user).await;
    let kept = started_at(&server, user, "connecting").await;
    assert_ne!(kept, first, "backdated value is what is stored now");
    assert_eq!(started_at(&server, user, "importing").await, kept);
    assert_eq!(started_at(&server, user, "discovering").await, kept);
    assert_eq!(started_at(&server, user, "done").await, kept);

    // done → discovering opens a new run.
    let restarted = started_at(&server, user, "discovering").await;
    assert_ne!(restarted, kept);

    // failed → discovering opens a new run too.
    started_at(&server, user, "failed").await;
    backdate_start(&server, user).await;
    let (_, listed) = get(&server, user, "/api/me/coding-agents/sync").await;
    let failed_start = listed["runs"][0]["started_at"].clone();
    assert_ne!(started_at(&server, user, "discovering").await, failed_start);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn started_at_stamps_on_a_first_row_in_discovering() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "fresh-user").await;

    assert!(started_at(&server, user, "discovering").await.is_string());

    server.cleanup().await;
}

async fn post(server: &common::TestServer, user: Uuid, path: &str) -> (StatusCode, String) {
    let res = as_user(server.client.post(server.url(path)), user)
        .send()
        .await
        .unwrap();
    let status = res.status();
    (status, res.text().await.unwrap())
}

fn v2_body() -> Value {
    let mut body = sync_body("sending");
    body["windows"] = json!([
        {"label": "Last 8 hours", "status": "done", "sessions": 12, "turns": 140},
        {"label": "Last day", "status": "running", "sessions": 3, "turns": 20}
    ]);
    let import = &mut body["agents"][0]["import"];
    import["pending"] = json!(7);
    import["rejected"] = json!(3);
    import["rejected_reasons"] = json!([{"reason": "payload too large", "count": 3}]);
    body
}

#[tokio::test]
#[serial]
async fn v1_body_is_stored_with_v2_defaults() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "v1-user").await;

    let stored = put_sync(&server, user, "dev", "importing").await;
    assert_eq!(stored["windows"], json!([]));
    assert_eq!(stored["retry_requested_at"], Value::Null);
    assert_eq!(
        stored["agents"],
        with_v2_defaults(sync_body("importing"))["agents"]
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn v2_fields_round_trip() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "v2-user").await;
    let body = v2_body();

    let (status, text) = put(&server, user, "/api/me/coding-agents/sync/dev", &body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let stored: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(stored["phase"], "sending");
    assert_eq!(stored["windows"], body["windows"]);
    assert_eq!(stored["agents"], body["agents"]);

    let (_, listed) = get(&server, user, "/api/me/coding-agents/sync").await;
    assert_eq!(listed["runs"][0], stored);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn v2_validation_failures_are_plain_text_400s() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "v2-bad").await;

    let mut too_many_windows = v2_body();
    let window = too_many_windows["windows"][0].clone();
    too_many_windows["windows"] = Value::Array(vec![window; 41]);
    let mut long_label = v2_body();
    long_label["windows"][0]["label"] = json!("l".repeat(65));
    let mut bad_window_status = v2_body();
    bad_window_status["windows"][0]["status"] = json!("failed");
    let mut negative_turns = v2_body();
    negative_turns["windows"][0]["turns"] = json!(-1);
    let mut too_many_reasons = v2_body();
    let reason = json!({"reason": "r", "count": 1});
    too_many_reasons["agents"][0]["import"]["rejected_reasons"] = Value::Array(vec![reason; 6]);
    let mut long_reason = v2_body();
    long_reason["agents"][0]["import"]["rejected_reasons"][0]["reason"] = json!("r".repeat(201));
    let mut negative_pending = v2_body();
    negative_pending["agents"][0]["import"]["pending"] = json!(-1);

    for (case, body) in [
        ("41 windows", too_many_windows),
        ("long window label", long_label),
        ("bad window status", bad_window_status),
        ("negative window turns", negative_turns),
        ("6 rejected reasons", too_many_reasons),
        ("long rejected reason", long_reason),
        ("negative pending", negative_pending),
    ] {
        let (status, text) = put(&server, user, "/api/me/coding-agents/sync/dev", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {text}");
        assert!(!text.is_empty(), "{case}: 400 must say why");
    }
    let (_, body) = get(&server, user, "/api/me/coding-agents/sync").await;
    assert_eq!(body, json!({"runs": []}), "nothing invalid was stored");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn retry_stamps_the_run_and_survives_later_reports() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "retry-user").await;

    let (status, text) = post(&server, user, "/api/me/coding-agents/sync/dev/retry").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    assert!(!text.is_empty());

    put_sync(&server, user, "dev", "sending").await;
    let (status, text) = post(&server, user, "/api/me/coding-agents/sync/dev/retry").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let run: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(run["device_id"], "dev");
    assert!(run["retry_requested_at"].is_string());

    // The next report does not clear the request: the client compares it to what it handled.
    let reported = put_sync(&server, user, "dev", "sending").await;
    assert_eq!(reported["retry_requested_at"], run["retry_requested_at"]);

    let (status, _) = post(&server, user, "/api/me/coding-agents/sync/bad.device/retry").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn retry_cannot_touch_another_users_device() {
    let server = common::TestServer::start().await;
    let alice = create_user(&server, "retry-alice").await;
    let bob = create_user(&server, "retry-bob").await;

    put_sync(&server, alice, "alice-device", "sending").await;
    let (status, _) = post(
        &server,
        bob,
        "/api/me/coding-agents/sync/alice-device/retry",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, runs) = get(&server, alice, "/api/me/coding-agents/sync").await;
    assert_eq!(runs["runs"][0]["retry_requested_at"], Value::Null);

    let res = server
        .client
        .post(server.url("/api/me/coding-agents/sync/alice-device/retry"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn consent_routes_and_table_are_gone() {
    let server = common::TestServer::start().await;
    let user = create_user(&server, "no-consent").await;

    let (status, _) = get(&server, user, "/api/me/coding-agents/consent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('coding_agent_consent')::text")
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(table, None);

    // Older clients still report `awaiting_consent`; it stays a valid phase.
    assert_eq!(
        put_sync(&server, user, "old", "awaiting_consent").await["phase"],
        "awaiting_consent"
    );

    server.cleanup().await;
}
