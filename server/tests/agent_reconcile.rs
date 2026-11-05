mod common;

use nasiko_server::agents::reconcile::reconcilable_agents;
use serial_test::serial;
use uuid::Uuid;

async fn owner(server: &common::TestServer) -> Uuid {
    sqlx::query_scalar("INSERT INTO users (username, email) VALUES ($1, $2) RETURNING id")
        .bind(format!("reconcile-{}", Uuid::new_v4().simple()))
        .bind(format!("rc-{}@test.local", Uuid::new_v4().simple()))
        .fetch_one(&server.db)
        .await
        .unwrap()
}

async fn agent(
    server: &common::TestServer,
    owner: Uuid,
    name: &str,
    status: &str,
    image: Option<&str>,
    internal: bool,
    deleted: bool,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, image, status, is_internal, deleted_at) \
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $6 THEN now() END) RETURNING id",
    )
    .bind(name)
    .bind(owner)
    .bind(image)
    .bind(status)
    .bind(internal)
    .bind(deleted)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

#[tokio::test]
#[serial]
async fn startup_reconcile_redeploys_only_ordinary_running_agents() {
    let server = common::TestServer::start().await;
    let owner = owner(&server).await;
    let ordinary = agent(
        &server,
        owner,
        "ordinary",
        "running",
        Some("x:1"),
        false,
        false,
    )
    .await;
    agent(
        &server,
        owner,
        "seeded-internal",
        "running",
        Some("x:1"),
        true,
        false,
    )
    .await;
    agent(
        &server,
        owner,
        "stopped",
        "stopped",
        Some("x:1"),
        false,
        false,
    )
    .await;
    agent(
        &server,
        owner,
        "deleted",
        "running",
        Some("x:1"),
        false,
        true,
    )
    .await;
    agent(&server, owner, "no-image", "running", None, false, false).await;

    let picked: Vec<(Uuid, String)> = reconcilable_agents(&server.db)
        .await
        .unwrap()
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();

    assert_eq!(picked, vec![(ordinary, "ordinary".to_string())]);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn an_internal_agent_is_left_to_its_seed_even_when_its_container_is_gone() {
    let server = common::TestServer::start().await;
    let owner = owner(&server).await;
    let internal = agent(
        &server,
        owner,
        "weave-dashboard-generator",
        "running",
        Some("nasiko/dynamic-ui:dev"),
        true,
        false,
    )
    .await;

    let picked: Vec<Uuid> = reconcilable_agents(&server.db)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();

    assert!(!picked.contains(&internal));
    assert!(picked.is_empty());
    server.cleanup().await;
}
