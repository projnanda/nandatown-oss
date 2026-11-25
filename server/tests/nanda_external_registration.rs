//! Connection ownership is independent of catalog sharing. The DB-only
//! fixtures avoid contacting a peer; the operator smoke exercises real A2A.
mod common;

use common::TestServer;
use serde_json::{Value, json};
use sqlx::Connection as _;
use uuid::Uuid;

const PATH: &str = "/api/nanda/external-agents";

async fn user(s: &TestServer, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO users (username,email) VALUES ($1,$2) RETURNING id")
        .bind(name)
        .bind(format!("{name}@test.local"))
        .fetch_one(&s.db)
        .await
        .unwrap()
}

async fn seed_connection(s: &TestServer, owner: Uuid) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO agents (owner_id,name,display_name) VALUES ($1,'local-id','Fixture') RETURNING id",
    ).bind(owner).fetch_one(&s.db).await.unwrap();
    sqlx::query("INSERT INTO nanda_external_connections (agent_id,owner_id,card_url,endpoint,auth_mode)
                 VALUES ($1,$2,'https://fixture.example/card','https://fixture.example/rpc','none')")
        .bind(id).bind(owner).execute(&s.db).await.unwrap();
    id
}

async fn get(s: &TestServer, caller: Uuid) -> Value {
    let res = common::as_member(s.client.get(s.url(PATH)), &caller.to_string(), "test")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    res.json().await.unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn catalog_grants_do_not_grant_connection_access_and_disconnect_retires_the_id() {
    let s = TestServer::start().await;
    let owner = user(&s, "peer-owner").await;
    let viewer = user(&s, "peer-viewer").await;
    assert_eq!(get(&s, owner).await, json!([]));
    let id = seed_connection(&s, owner).await;
    sqlx::query("UPDATE agents SET is_public=true WHERE id=$1")
        .bind(id)
        .execute(&s.db)
        .await
        .unwrap();
    assert_eq!(get(&s, viewer).await, json!([]));
    assert_eq!(
        get(&s, owner).await,
        json!([{
            "id":id, "name":"Fixture", "card_url":"https://fixture.example/card",
            "endpoint":"https://fixture.example/rpc", "auth_mode":"none"
        }])
    );
    let path = format!("{PATH}/{id}");
    let invoke = s
        .client
        .post(s.url(&format!("{path}/tasks")))
        .json(&json!({"text":"hello"}));
    for request in [invoke, s.client.delete(s.url(&path))] {
        // Even a platform superuser who is not the connection owner gets no access.
        let res = common::as_superuser(request, &viewer.to_string(), "peer-viewer")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }
    // This is the lock an admitted call holds while its peer is answering.
    // Use a distinct connection: the stock test server has a one-slot pool.
    let mut other = sqlx::PgConnection::connect_with(&s.db.connect_options())
        .await
        .unwrap();
    let mut admitted = other.begin().await.unwrap();
    sqlx::query("SELECT id FROM agents WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *admitted)
        .await
        .unwrap();
    let next = s
        .client
        .post(s.url(&format!("{path}/tasks")))
        .json(&json!({"text":"hello"}));
    for request in [next, s.client.delete(s.url(&path))] {
        let res = common::as_member(request, &owner.to_string(), "peer-owner")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 409);
    }
    admitted.rollback().await.unwrap();
    other.close().await.unwrap();
    let res = common::as_member(
        s.client.delete(s.url(&path)),
        &owner.to_string(),
        "peer-owner",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(get(&s, owner).await, json!([]));
    let retained: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM nanda_external_connections WHERE agent_id=$1)",
    )
    .bind(id)
    .fetch_one(&s.db)
    .await
    .unwrap();
    assert!(!retained);
    let retired: bool = sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM agents WHERE id=$1")
        .bind(id)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert!(retired);
    // Generic catalog soft deletion must wipe the separate binding too;
    // restoring the catalog record can never restore prior outbound authority.
    let second = seed_connection(&s, owner).await;
    sqlx::query("UPDATE agents SET deleted_at=now() WHERE id=$1")
        .bind(second)
        .execute(&s.db)
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET deleted_at=NULL WHERE id=$1")
        .bind(second)
        .execute(&s.db)
        .await
        .unwrap();
    assert_eq!(get(&s, owner).await, json!([]));
    s.cleanup().await;
}

#[tokio::test]
#[serial_test::serial]
async fn repurposing_a_catalog_row_removes_external_disconnect_authority() {
    let s = TestServer::start().await;
    let owner = user(&s, "repurpose-owner").await;
    let id = seed_connection(&s, owner).await;
    // The generic catalog can rehost an agent. A subsequent external
    // disconnect must not soft-delete a now-managed, running workload.
    sqlx::query("UPDATE agents SET status='running',url='http://managed.invalid/' WHERE id=$1")
        .bind(id)
        .execute(&s.db)
        .await
        .unwrap();
    let res = common::as_member(
        s.client.delete(s.url(&format!("{PATH}/{id}"))),
        &owner.to_string(),
        "repurpose-owner",
    )
    .send()
    .await
    .unwrap();
    let intact: bool = sqlx::query_scalar("SELECT deleted_at IS NULL FROM agents WHERE id=$1")
        .bind(id)
        .fetch_one(&s.db)
        .await
        .unwrap();
    s.cleanup().await;
    assert_eq!(res.status(), 404);
    assert!(intact);
}

#[tokio::test]
#[serial_test::serial]
async fn registration_rejects_the_current_oasis_session_as_a_peer_credential() {
    let s = TestServer::start().await;
    let owner = user(&s, "peer-token-owner").await;
    let session = common::sign_token(&owner.to_string(), "peer-token-owner", false, "member");
    let res = s.client.post(s.url(PATH)).bearer_auth(&session).json(&json!({
        "card_url":"https://fixture.example/card", "expected_endpoint":"https://fixture.example/rpc",
        "auth_mode":"bearer", "peer_token":session
    })).send().await.unwrap();
    let status = res.status();
    let result: Value = res.json().await.unwrap();
    s.cleanup().await;
    assert_eq!(status, 400);
    assert_eq!(result, json!({"error":"invalid_peer_auth"}));
}
