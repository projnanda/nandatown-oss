use std::time::Duration;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use nasiko_secrets::SecretsCrypto;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, SemaphorePermit};
use uuid::Uuid;

use super::{net, wire};
use crate::{auth::Claims, rate_limit::RateLimiter, state::AppState};

// Global PER PROCESS: at most two connections can be held by remote work.
// Refuse immediately instead of accumulating waiting HTTP tasks/DB leases.
static OUTBOUND_SLOTS: Semaphore = Semaphore::const_new(2);
const MAX_TEXT: usize = 8 * 1024;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/nanda/external-agents", get(list).post(register))
        .route("/nanda/external-agents/preview", post(preview))
        .route("/nanda/external-agents/{id}/tasks", post(task))
        .route("/nanda/external-agents/{id}", delete(disconnect))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(middleware::from_fn_with_state(
            RateLimiter::new(30, Duration::from_secs(60)),
            limit_owner,
        ))
        .layer(middleware::map_response(|mut res: Response| async {
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }))
}

async fn limit_owner(
    State(limiter): State<RateLimiter>,
    claims: Claims,
    request: Request,
    next: middleware::Next,
) -> Response {
    if let Err(wait) = limiter.check(&claims.sub) {
        let mut response = Failure(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response();
        if let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    next.run(request).await
}

struct Failure(StatusCode, &'static str);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error":self.1}))).into_response()
    }
}
type Out<T> = Result<T, Failure>;
fn owner(claims: &Claims) -> Out<Uuid> {
    claims
        .user_uuid()
        .map_err(|_| Failure(StatusCode::UNAUTHORIZED, "invalid_user"))
}
fn body<T>(input: Result<Json<T>, JsonRejection>) -> Out<T> {
    // Axum's detailed deserialization diagnostics can include a synthetic
    // unknown field or a malformed value, including a pasted credential.
    input.map(|Json(v)| v).map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Failure(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large")
        } else {
            Failure(StatusCode::BAD_REQUEST, "invalid_request")
        }
    })
}
fn outbound_slot() -> Out<SemaphorePermit<'static>> {
    OUTBOUND_SLOTS
        .try_acquire()
        .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE, "a2a_capacity"))
}
fn db_error(err: sqlx::Error) -> Failure {
    match err.as_database_error().and_then(|e| e.code()).as_deref() {
        Some("55P03") => Failure(StatusCode::CONFLICT, "connection_busy"),
        Some("23505") => Failure(StatusCode::CONFLICT, "connection_exists"),
        _ => Failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "connection_store_unavailable",
        ),
    }
}
fn crypto_unavailable() -> Failure {
    Failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "peer_credential_unavailable",
    )
}
fn external_error(code: &'static str) -> Failure {
    let status = match code {
        "a2a_disabled" | "a2a_config_invalid" | "a2a_target_unavailable" => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        "a2a_target_not_allowed"
        | "a2a_endpoint_not_allowed"
        | "a2a_security_unsupported"
        | "a2a_card_unsupported"
        | "a2a_card_invalid" => StatusCode::BAD_REQUEST,
        "a2a_task_pending" | "a2a_task_unconfirmed" => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::BAD_GATEWAY,
    };
    Failure(status, code)
}
fn endpoint_changed() -> Failure {
    Failure(StatusCode::CONFLICT, "a2a_endpoint_changed")
}
fn absent() -> Failure {
    Failure(StatusCode::NOT_FOUND, "connection_not_found")
}
fn auth_invalid() -> Failure {
    Failure(StatusCode::BAD_REQUEST, "invalid_peer_auth")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CardInput {
    card_url: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    card_url: String,
    expected_endpoint: String,
    auth_mode: String,
    peer_token: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskInput {
    text: String,
}

#[derive(Serialize, sqlx::FromRow)]
struct Connection {
    id: Uuid,
    name: String,
    card_url: String,
    endpoint: String,
    auth_mode: String,
}

#[derive(Serialize)]
struct Preview {
    name: String,
    endpoint: String,
    requires_bearer: bool,
}

async fn checked_card(raw: &str) -> Out<(reqwest::Client, reqwest::Url, wire::Card)> {
    let (client, card_url, approved) = net::target(raw).await.map_err(external_error)?;
    let card = wire::inspect_card(&client, &card_url)
        .await
        .map_err(external_error)?;
    if card.endpoint != approved {
        return Err(endpoint_changed());
    }
    Ok((client, card_url, card))
}

async fn list(State(s): State<AppState>, claims: Claims) -> Out<Json<Vec<Connection>>> {
    let owner_id = owner(&claims)?;
    let rows = sqlx::query_as(
        "SELECT a.id, COALESCE(a.display_name,a.name) AS name, c.card_url,c.endpoint,c.auth_mode
         FROM nanda_external_connections c JOIN agents a ON a.id=c.agent_id
         WHERE c.owner_id=$1 AND a.owner_id=$1 AND a.deleted_at IS NULL ORDER BY name,a.id",
    )
    .bind(owner_id)
    .fetch_all(&s.db)
    .await
    .map_err(db_error)?;
    Ok(Json(rows))
}

async fn preview(
    claims: Claims,
    input: Result<Json<CardInput>, JsonRejection>,
) -> Out<Json<Preview>> {
    owner(&claims)?;
    let input = body(input)?;
    let _permit = outbound_slot()?;
    let (_, _, card) = checked_card(&input.card_url).await?;
    Ok(Json(Preview {
        name: card.name,
        endpoint: card.endpoint.to_string(),
        requires_bearer: card.requires_bearer,
    }))
}

// Domain separation from standard agent secrets, with destination and owner
// also authenticated. Copying the ciphertext to another endpoint cannot work.
fn credential_aad(owner_id: Uuid, endpoint: &str) -> Vec<u8> {
    format!("nanda/external-peer/v1\0{owner_id}\0{endpoint}").into_bytes()
}
async fn register(
    State(s): State<AppState>,
    claims: Claims,
    headers: HeaderMap,
    input: Result<Json<Registration>, JsonRejection>,
) -> Out<(StatusCode, Json<Connection>)> {
    let owner_id = owner(&claims)?;
    let input = body(input)?;
    let token = input.peer_token.as_deref();
    if !credentials_match(&input.auth_mode, token, false)
        || token
            .is_some_and(|t| crate::auth::middleware::extract_token(&headers).as_deref() == Some(t))
    {
        return Err(auth_invalid());
    }
    let _permit = outbound_slot()?;
    let (_, card_url, card) = checked_card(&input.card_url).await?;
    let endpoint = card.endpoint.as_str();
    if input.expected_endpoint != endpoint {
        return Err(endpoint_changed());
    }
    if !credentials_match(&input.auth_mode, token, card.requires_bearer) {
        return Err(auth_invalid());
    }
    let id = Uuid::new_v4();
    let sealed = token
        .map(|t| {
            SecretsCrypto::try_for_agent(id)
                .map(|c| c.encrypt_with_aad(t, &credential_aad(owner_id, endpoint)))
                .map_err(|_| crypto_unavailable())
        })
        .transpose()?;
    let mut tx = s.db.begin().await.map_err(db_error)?;
    let slug = format!("external-{id}");
    s.agent_admission
        .admit(&mut tx, owner_id, &slug)
        .await
        .map_err(|(status, _)| Failure(status, "catalog_admission_refused"))?;
    // Stable unique internal slug; no runtime address, loadable peer card,
    // credentials, deployment request, or generic access grants on the agent.
    sqlx::query(
        "INSERT INTO agents (id,owner_id,name,display_name,is_public,status,description,
                             protocol_version,default_input_modes,default_output_modes,tags,metadata)
         VALUES ($1,$2,$3,$4,false,'registered',
                 'Independently operated A2A agent. Use the External agents page to send a text task; the catalog URL is not an invocation endpoint.',
                 '1.0','[\"text/plain\"]','[\"text/plain\"]',ARRAY['nanda-external-a2a'],$5)",
    )
    .bind(id)
    .bind(owner_id)
    .bind(slug)
    .bind(&card.name)
    .bind(serde_json::json!({"nanda_external":{
        "operator_page":"/nanda/external-agents",
        "task_path":format!("/api/nanda/external-agents/{id}/tasks"),
        "managed_proxy":false
    }}))
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    sqlx::query(
        "INSERT INTO nanda_external_connections
                 (agent_id,owner_id,card_url,endpoint,auth_mode,peer_token_enc)
                 VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(owner_id)
    .bind(card_url.as_str())
    .bind(endpoint)
    .bind(&input.auth_mode)
    .bind(sealed)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    Ok((
        StatusCode::CREATED,
        Json(Connection {
            id,
            name: card.name,
            card_url: card_url.to_string(),
            endpoint: endpoint.to_owned(),
            auth_mode: input.auth_mode,
        }),
    ))
}

// Lock the catalog row too: a successful generic soft delete / ownership
// change cannot race an admitted send. The lock lives until the bounded
// remote call returns. Two concurrent tasks for one connection do not queue.
#[derive(sqlx::FromRow)]
struct Binding {
    card_url: String,
    endpoint: String,
    auth_mode: String,
    peer_token_enc: Option<String>,
}
async fn lock_binding(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    owner_id: Uuid,
) -> Out<Binding> {
    sqlx::query_as(
        "SELECT c.card_url,c.endpoint,c.auth_mode,c.peer_token_enc
                    FROM agents a JOIN nanda_external_connections c ON c.agent_id=a.id
                    WHERE a.id=$1 AND a.owner_id=$2 AND c.owner_id=$2 AND a.deleted_at IS NULL
                    FOR UPDATE OF a,c NOWAIT",
    )
    .bind(id)
    .bind(owner_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error)?
    .ok_or_else(absent)
}

async fn task(
    State(s): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
    input: Result<Json<TaskInput>, JsonRejection>,
) -> Out<Json<wire::Answer>> {
    let owner_id = owner(&claims)?;
    let input = body(input)?;
    if input.text.trim().is_empty() || input.text.len() > MAX_TEXT {
        return Err(Failure(StatusCode::BAD_REQUEST, "invalid_task_text"));
    }
    let _permit = outbound_slot()?;
    let mut tx = s.db.begin().await.map_err(db_error)?;
    let binding = lock_binding(&mut tx, id, owner_id).await?;
    let (client, _, card) = checked_card(&binding.card_url).await?;
    if card.endpoint.as_str() != binding.endpoint {
        return Err(endpoint_changed());
    }
    let token = match (binding.auth_mode.as_str(), binding.peer_token_enc) {
        ("none", None) if !card.requires_bearer => None,
        ("none", None) => return Err(Failure(StatusCode::CONFLICT, "peer_auth_changed")),
        ("bearer", Some(enc)) => Some(
            SecretsCrypto::try_for_agent(id)
                .and_then(|c| {
                    c.decrypt_with_aad(&enc, &credential_aad(owner_id, &binding.endpoint))
                })
                .map_err(|_| crypto_unavailable())?,
        ),
        _ => return Err(crypto_unavailable()),
    };
    let answer = wire::invoke(&client, &card.endpoint, token.as_deref(), &input.text)
        .await
        .map_err(external_error)?;
    // Read-only apart from locks. Do not turn a release error after a peer
    // has answered into an invitation to resubmit a task.
    drop(tx);
    Ok(Json(answer))
}

async fn disconnect(
    State(s): State<AppState>,
    claims: Claims,
    Path(id): Path<Uuid>,
) -> Out<StatusCode> {
    let owner_id = owner(&claims)?;
    let mut tx = s.db.begin().await.map_err(db_error)?;
    lock_binding(&mut tx, id, owner_id).await?;
    sqlx::query(
        "UPDATE agents SET deleted_at=now(),updated_at=now()
                 WHERE id=$1 AND owner_id=$2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(owner_id)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    // The migration's trigger destroys the binding+secret in this same
    // transaction, including when a caller uses the generic catalog delete.
    tx.commit().await.map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn credentials_match(mode: &str, token: Option<&str>, required: bool) -> bool {
    match (mode, token) {
        ("none", None) => !required,
        ("bearer", Some(t)) if t.len() <= 4096 => {
            let value = t.trim_end_matches('=');
            !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_explicitly_chosen_peer_auth() {
        assert!(credentials_match("none", None, false));
        assert!(credentials_match("bearer", Some("example.token-1"), false));
        assert!(credentials_match("bearer", Some("example+token/2=="), true));
        assert!(!credentials_match("none", None, true));
        assert!(!credentials_match("none", Some("example"), false));
        for token in [
            None,
            Some(""),
            Some(" leading"),
            Some("embedded\nnewline"),
            Some("a=b"),
        ] {
            assert!(!credentials_match("bearer", token, false));
        }
        assert!(!credentials_match("bearer", Some(&"a".repeat(4097)), true));
        assert!(!credentials_match("unknown", None, false));
    }
}
