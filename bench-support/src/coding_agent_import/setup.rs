//! Users for the run, made through the server's own API the way an operator
//! and the CLI would: the admin creates each user, the user signs in with the
//! one-time access key, and registers its coding-agent harness
//! (`POST /api/agents/coding-integrations`, as `nasiko agents setup` does).

use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::history::INTEGRATION_ID;

/// Login is rate limited per server (30/min, one shared bucket); a run signs in
/// once per user, so a large run waits for the window rather than failing.
const LOGIN_ATTEMPTS: u32 = 10;
const HEALTH_WAIT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct User {
    pub index: usize,
    pub id: Uuid,
    pub username: String,
    pub token: String,
    /// The harness agent's name, which every turn names as its source.
    pub agent_name: String,
}

/// Waits for `/health` (the server runs its migrations first).
pub async fn wait_for_server(http: &reqwest::Client, base: &str) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + HEALTH_WAIT;
    loop {
        let healthy = http
            .get(format!("{base}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if healthy {
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            bail!("server at {base} not healthy after {HEALTH_WAIT:?}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Signs in, waiting out the login rate limit if it is hit.
pub async fn login(http: &reqwest::Client, base: &str, body: Value) -> anyhow::Result<Value> {
    for _ in 0..LOGIN_ATTEMPTS {
        let response = http
            .post(format!("{base}/api/auth/login"))
            .json(&body)
            .send()
            .await?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let wait = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()?.parse().ok())
                .unwrap_or(10);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        let status = response.status();
        let value: Value = response.json().await.context("login response")?;
        if !status.is_success() {
            bail!("login failed: HTTP {status}: {value}");
        }
        return Ok(value);
    }
    bail!("login still rate limited after {LOGIN_ATTEMPTS} attempts")
}

pub async fn admin_token(
    http: &reqwest::Client,
    base: &str,
    username: &str,
    password: &str,
) -> anyhow::Result<String> {
    let session = login(
        http,
        base,
        json!({"username": username, "password": password}),
    )
    .await?;
    token_of(&session)
}

/// Creates, signs in and registers user `index` of run `run_id`.
pub async fn create_user(
    http: &reqwest::Client,
    base: &str,
    admin: &str,
    run_id: &str,
    index: usize,
) -> anyhow::Result<User> {
    let username = format!("lt-{run_id}-u{index:02}");
    let created = http
        .post(format!("{base}/api/users"))
        .bearer_auth(admin)
        .json(&json!({
            "username": username,
            "email": format!("{username}@loadtest.invalid"),
            "display_name": format!("Load test user {index}"),
        }))
        .send()
        .await?;
    let status = created.status();
    let created: Value = created.json().await.context("create-user response")?;
    if status != StatusCode::CREATED {
        bail!("creating {username}: HTTP {status}: {created}");
    }
    let field = |name: &str| {
        created
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("create-user response has no {name}"))
    };
    let session = login(
        http,
        base,
        json!({"access_key": field("access_key")?, "access_secret": field("access_secret")?}),
    )
    .await?;
    let token = token_of(&session)?;
    let id = Uuid::parse_str(&field("id")?)?;
    let agent_name = register_harness(http, base, &token).await?;
    Ok(User {
        index,
        id,
        username,
        token,
        agent_name,
    })
}

async fn register_harness(
    http: &reqwest::Client,
    base: &str,
    token: &str,
) -> anyhow::Result<String> {
    let response = http
        .post(format!("{base}/api/agents/coding-integrations"))
        .bearer_auth(token)
        .json(&json!({"integration_id": INTEGRATION_ID}))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        bail!("registering the harness: HTTP {status}: {body}");
    }
    let value: Value = serde_json::from_str(&body)?;
    value
        .get("name")
        .or_else(|| value.pointer("/data/name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("registration response has no name: {body}"))
}

fn token_of(session: &Value) -> anyhow::Result<String> {
    session
        .get("token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("login response has no token: {session}"))
}
