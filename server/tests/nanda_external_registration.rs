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

#[tokio::test]
#[serial_test::serial]
async fn another_signed_town_session_is_not_a_peer_credential() {
    let s = TestServer::start().await;
    let owner = user(&s, "session-holder").await;
    let other = common::sign_token(&Uuid::new_v4().to_string(), "other", false, "member");
    let res = common::as_member(s.client.post(s.url(PATH)), &owner.to_string(), "session-holder")
        .json(&json!({"card_url":"https://fixture.example/card",
            "expected_endpoint":"https://fixture.example/rpc","auth_mode":"bearer","peer_token":other}))
        .send().await.unwrap();
    let result: Value = res.json().await.unwrap();
    s.cleanup().await;
    assert_eq!(result, json!({"error":"invalid_peer_auth"}));
}

#[tokio::test]
#[serial_test::serial]
async fn peer_credentials_are_distinguished_by_issuer_not_jwt_shape() {
    let runtime_key = "fixture-runtime-key";
    let s = TestServer::start_with(|c| {
        c.agent_jwt_secret = runtime_key.into();
        c.agent_jwt_algorithm = "HS384".into();
    })
    .await;
    let owner = user(&s, "issuer-boundary").await;
    let identity = nasiko_auth::Identity {
        user_id: owner.to_string(),
        username: "issuer-boundary".into(),
        is_superuser: false,
    };
    let service = |key| nasiko_auth::jwt::encode_agent_jwt(key, 60, &identity).unwrap();
    let delegated = nasiko_auth::jwt::encode_delegated_jwt(
        common::TEST_JWT_SECRET,
        60,
        &identity,
        "fixture-audience",
        &["me:read".into()],
        "fixture-parent",
    )
    .unwrap()
    .0;
    let llm = nasiko_llm_router::auth::mint_agent_token(
        "fixture",
        &owner.to_string(),
        runtime_key,
        60,
        jsonwebtoken::Algorithm::HS384,
    )
    .unwrap();
    let mut outcomes = Vec::new();
    for (token, should_refuse) in [
        (service(common::TEST_JWT_SECRET), true),
        (delegated, true),
        (llm, true),
        (format!("ngt_{}", "12".repeat(32)), true),
        (format!("nst_{}", "ab".repeat(32)), true),
        (service("independent-fixture-key"), false),
        ("ngt_peer-defined-value".into(), false),
    ] {
        // An empty card URL cannot start a peer request, whatever the allowlist.
        let r = common::as_member(s.client.post(s.url(PATH)), &owner.to_string(), "issuer-boundary")
            .json(&json!({"card_url":"", "expected_endpoint":"", "auth_mode":"bearer","peer_token":token}))
            .send().await.unwrap();
        let body: Value = r.json().await.unwrap();
        outcomes.push((body["error"].as_str().unwrap().to_owned(), should_refuse));
    }
    s.cleanup().await;
    for (error, should_refuse) in outcomes {
        if should_refuse {
            assert_eq!(error, "invalid_peer_auth");
        } else {
            assert!(matches!(
                error.as_str(),
                "a2a_disabled" | "a2a_target_not_allowed"
            ));
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn nonlocal_bearer_registration_requires_a_trusted_tls_ingress() {
    let s = TestServer::start_with(|c| c.bind = "0.0.0.0:8080".into()).await;
    let owner = user(&s, "transport").await;
    let res = common::as_member(s.client.post(s.url(PATH)), &owner.to_string(), "transport")
        .header("X-Forwarded-Proto", "https")
        .json(&json!({"card_url":"https://fixture.example/card",
            "expected_endpoint":"https://fixture.example/rpc","auth_mode":"bearer","peer_token":"fixture"}))
        .send().await.unwrap();
    let result: Value = res.json().await.unwrap();
    s.cleanup().await;
    assert_eq!(result, json!({"error":"secure_transport_required"}));
}

#[tokio::test]
#[serial_test::serial]
async fn an_active_external_agent_never_enters_the_managed_runtime() {
    let s = TestServer::start().await;
    let owner = user(&s, "managed-refusal").await;
    let id = seed_connection(&s, owner).await;
    let mut codes = Vec::new();
    for name in ["local-id".to_string(), id.to_string()] {
        let res = common::as_superuser(
            s.client.post(s.url("/api/containers")),
            &owner.to_string(),
            "managed-refusal",
        )
        .json(&json!({"name":name, "image":"example/fixture:1"}))
        .send()
        .await
        .unwrap();
        codes.push(res.status().as_u16());
    }
    let called = s.runtime.deployed_specs().len();
    s.cleanup().await;
    assert_eq!(codes, [409, 409]);
    assert_eq!(called, 0);
}

#[tokio::test]
#[serial_test::serial]
async fn retired_external_identity_cannot_fall_back_to_an_ad_hoc_container() {
    let s = TestServer::start().await;
    let owner = user(&s, "retired-refusal").await;
    let id = seed_connection(&s, owner).await;
    let slug = format!("external-{id}");
    sqlx::query("UPDATE agents SET name=$2, deleted_at=now() WHERE id=$1")
        .bind(id)
        .bind(&slug)
        .execute(&s.db)
        .await
        .unwrap();
    let mut codes = Vec::new();
    for name in [slug, id.to_string()] {
        let res = common::as_superuser(
            s.client.post(s.url("/api/containers")),
            &owner.to_string(),
            "retired-refusal",
        )
        .json(&json!({"name":name, "image":"example/fixture:1"}))
        .send()
        .await
        .unwrap();
        codes.push(res.status().as_u16());
    }
    let called = s.runtime.deployed_specs().len();
    s.cleanup().await;
    assert_eq!(codes, [404, 404]);
    assert_eq!(called, 0);
}

#[tokio::test]
#[serial_test::serial]
async fn ordinary_catalog_and_ad_hoc_deploys_keep_their_runtime_identity() {
    let s = TestServer::start().await;
    let owner = user(&s, "ordinary-deployer").await;
    // An owner-written metadata flag is not the external binding.
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, metadata)
        VALUES ('ordinary', $1, '{\"nanda_external\":{}}') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&s.db)
    .await
    .unwrap();
    for name in ["ordinary", "ad-hoc"] {
        let r = common::as_superuser(
            s.client.post(s.url("/api/containers")),
            &owner.to_string(),
            "ordinary-deployer",
        )
        .json(&json!({"name":name,"image":"example/fixture:1"}))
        .send()
        .await
        .unwrap();
        assert_eq!(r.status(), 201);
    }
    let keys: Vec<_> = s
        .runtime
        .deployed_specs()
        .iter()
        .map(|spec| spec.container_id.as_str().to_owned())
        .collect();
    s.cleanup().await;
    assert_eq!(keys, [id.to_string(), "ad-hoc".to_owned()]);
}

#[tokio::test]
#[serial_test::serial]
async fn catalog_detail_recognizes_only_an_active_owned_binding_and_retires_generated_hints() {
    let s = TestServer::start().await;
    let owner = user(&s, "detail-owner").await;
    let viewer = user(&s, "detail-viewer").await;
    let id = seed_connection(&s, owner).await;
    sqlx::query("UPDATE agents SET is_public=true, metadata='{\"nanda_external\":{},\"keep\":true}', tags=ARRAY['nanda-external-a2a','custom'] WHERE id=$1")
        .bind(id).execute(&s.db).await.unwrap();
    let path = format!("/api/agents/{id}");
    let detail =
        |who: Uuid| common::as_member(s.client.get(s.url(&path)), &who.to_string(), "detail");
    let owned: Value = detail(owner).send().await.unwrap().json().await.unwrap();
    let shared: Value = detail(viewer).send().await.unwrap().json().await.unwrap();
    sqlx::query("UPDATE agents SET owner_id=$2 WHERE id=$1")
        .bind(id)
        .bind(viewer)
        .execute(&s.db)
        .await
        .unwrap();
    let transferred: Value = detail(viewer).send().await.unwrap().json().await.unwrap();
    s.cleanup().await;
    assert_eq!(owned["data"]["is_external_connection"], true);
    assert_eq!(shared["data"]["is_external_connection"], true);
    assert_eq!(transferred["data"]["is_external_connection"], false);
    assert_eq!(transferred["data"]["metadata"], json!({"keep":true}));
    assert_eq!(transferred["data"]["tags"], json!(["custom"]));
}

#[tokio::test]
#[serial_test::serial]
async fn disconnect_stops_the_record_and_tombstones_its_platform_credentials() {
    use nasiko_mcp_gateway::agent_tokens::{TokenScope, mint_scoped};
    #[derive(Default)]
    struct Retired(std::sync::Mutex<Vec<Uuid>>);
    #[async_trait::async_trait]
    impl nasiko_server::agent_lifecycle::AgentDeletionHook for Retired {
        async fn on_agent_deleted(&self, id: Uuid) {
            self.0.lock().unwrap().push(id);
        }
    }
    let s = TestServer::start().await;
    let retired = std::sync::Arc::new(Retired::default());
    s.state.agent_deletion_hook.install(retired.clone());
    let owner = user(&s, "disconnect-owner").await;
    let id = seed_connection(&s, owner).await;
    for scope in [TokenScope::Gateway, TokenScope::State] {
        mint_scoped(&s.db, id, scope).await.unwrap();
    }
    let res = common::as_member(
        s.client.delete(s.url(&format!("{PATH}/{id}"))),
        &owner.to_string(),
        "disconnect-owner",
    )
    .send()
    .await
    .unwrap();
    let row: (String,bool,bool) = sqlx::query_as(
        "SELECT a.status, g.revoked_at IS NOT NULL, s.revoked_at IS NOT NULL FROM agents a
          JOIN agent_gateway_tokens g ON g.agent_id=a.id JOIN agent_state_tokens s ON s.agent_id=a.id WHERE a.id=$1")
        .bind(id).fetch_one(&s.db).await.unwrap();
    s.cleanup().await;
    assert_eq!(res.status(), 204);
    assert_eq!(row, ("stopped".to_string(), true, true));
    assert_eq!(*retired.0.lock().unwrap(), [id]);
}

#[tokio::test]
#[serial_test::serial]
async fn generic_delete_tears_down_only_managed_agents() {
    let s = TestServer::start().await;
    let owner = user(&s, "generic-owner").await;
    let external = seed_connection(&s, owner).await;
    let managed: Uuid = sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id) VALUES ('managed', $1) RETURNING id",
    )
    .bind(owner)
    .fetch_one(&s.db)
    .await
    .unwrap();
    let mut stopped = Vec::new();
    for id in [external, managed] {
        let res = common::as_member(
            s.client.delete(s.url(&format!("/api/agents/{id}"))),
            &owner.to_string(),
            "generic-owner",
        )
        .send()
        .await
        .unwrap();
        assert_eq!(res.status(), 200);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["runtime_errors"], json!([]));
        stopped.push(body["containers_stopped"].as_u64().unwrap());
    }
    s.cleanup().await;
    // This runtime reports success for each destroy. An unbound agent checks
    // both its UUID-keyed container and the legacy name fallback.
    assert_eq!(stopped, [0, 2]);
}

#[tokio::test]
#[serial_test::serial]
async fn retirement_winning_the_source_update_claim_cannot_queue_a_managed_build() {
    let s = TestServer::start().await;
    let owner = user(&s, "update-owner").await;
    let id = seed_connection(&s, owner).await;
    let mut other = sqlx::PgConnection::connect_with(&s.db.connect_options())
        .await
        .unwrap();
    let mut retire = other.begin().await.unwrap();
    sqlx::query("SELECT id FROM agents WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *retire)
        .await
        .unwrap();
    let req = common::as_superuser(
        s.client.put(s.url(&format!("/api/agents/{id}/update"))),
        &owner.to_string(),
        "update-owner",
    )
    .multipart(reqwest::multipart::Form::new().text("version", "2.0.0"));
    let updating = tokio::spawn(async move { req.send().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND pg_backend_pid() = ANY(pg_blocking_pids(pid)))")
                .fetch_one(&mut *retire).await.unwrap();
            if blocked { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("source update reaches the locked catalog row");
    sqlx::query("UPDATE agents SET deleted_at=now(), status='stopped' WHERE id=$1")
        .bind(id)
        .execute(&mut *retire)
        .await
        .unwrap();
    retire.commit().await.unwrap();
    other.close().await.unwrap();
    let res = updating.await.unwrap().unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_builds WHERE agent_id=$1")
        .bind(id)
        .fetch_one(&s.db)
        .await
        .unwrap();
    s.cleanup().await;
    assert_eq!(res.status(), 404);
    assert_eq!(count, 0);
}

#[tokio::test]
#[serial_test::serial]
async fn authorized_source_conversion_retires_the_binding_and_queues_the_build() {
    let s = TestServer::start().await;
    let owner = user(&s, "convert-owner").await;
    let id = seed_connection(&s, owner).await;
    let req = common::as_superuser(
        s.client.put(s.url(&format!("/api/agents/{id}/update"))),
        &owner.to_string(),
        "convert-owner",
    )
    .multipart(
        reqwest::multipart::Form::new()
            .text("version", "2.0.0")
            .text("parking_policy", "resident"),
    );
    let res = req.send().await.unwrap();
    let row: (bool, i64, String) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM nanda_external_connections WHERE agent_id=$1),
           (SELECT COUNT(*) FROM agent_builds WHERE agent_id=$1), parking_policy FROM agents WHERE id=$1")
        .bind(id).fetch_one(&s.db).await.unwrap();
    s.cleanup().await;
    assert_eq!(res.status(), 202);
    assert_eq!(row, (false, 1, "resident".into()));
}

#[tokio::test]
#[serial_test::serial]
async fn external_task_lock_does_not_occupy_the_ordinary_pool() {
    let s = TestServer::start().await;
    let owner = user(&s, "capacity-owner").await;
    let id = seed_connection(&s, owner).await;
    let mut observer = sqlx::PgConnection::connect_with(&s.db.connect_options())
        .await
        .unwrap();
    let mut held = observer.begin().await.unwrap();
    sqlx::query("LOCK TABLE nanda_external_connections IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *held)
        .await
        .unwrap();
    let req = common::as_member(
        s.client.post(s.url(&format!("{PATH}/{id}/tasks"))),
        &owner.to_string(),
        "capacity-owner",
    )
    .json(&json!({"text":"hello"}));
    let pending = tokio::spawn(async move { req.send().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE NOT granted AND relation='nanda_external_connections'::regclass)")
                .fetch_one(&mut *held).await.unwrap();
            if waiting { break }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("task reaches binding lock");
    let reserve = matches!(
        tokio::time::timeout(std::time::Duration::from_millis(250), s.db.acquire()).await,
        Ok(Ok(_))
    );
    held.rollback().await.unwrap();
    pending.await.unwrap().unwrap();
    observer.close().await.unwrap();
    s.cleanup().await;
    assert!(
        reserve,
        "unrelated requests must retain ordinary DB capacity"
    );
}
