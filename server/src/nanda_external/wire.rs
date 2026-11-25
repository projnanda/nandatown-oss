//! Narrow A2A 1.0 JSON-RPC binding. The caller owns destination approval,
//! address pinning, and a client with redirects, retries and ambient proxies off.
use reqwest::{Client, Url};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::future::Future;
use std::time::Duration;

const CARD_INVALID: &str = "a2a_card_invalid";
const CARD_UNSUPPORTED: &str = "a2a_card_unsupported";
const SECURITY: &str = "a2a_security_unsupported";
const BYTES: usize = 128 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
// Leave room for two in-flight tasks on the fixture's shared 30 RPC/min budget.
// This paces polling; it is not a per-peer quota across owners or replicas.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const TEXT_BYTES: usize = 16 * 1024;
const UNKNOWN: &str = "a2a_submission_unknown";
const UNCONFIRMED: &str = "a2a_task_unconfirmed";
const INVALID: &str = "a2a_invalid_result";

#[derive(Debug)]
pub(super) struct Card {
    pub name: String,
    pub endpoint: Url,
    pub requires_bearer: bool,
}

fn optional_array<'a>(object: &'a Value, key: &str) -> Option<&'a [Value]> {
    match object.get(key) {
        None => Some(&[]),
        Some(v) => v.as_array().map(Vec::as_slice),
    }
}

fn bounded_label(v: &Value) -> Option<&str> {
    let s = v.as_str()?;
    (!s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)).then_some(s)
}

fn check_text_modes(card: &Value, field: &str) -> Result<(), &'static str> {
    use mime_guess::mime;
    let Some(declared) = card.get(field) else {
        // Some peers omit the defaults. There is no incompatible claim to reject;
        // SendMessage still requests text/plain and only actual text is returned.
        return Ok(());
    };
    let mut compatible = false;
    for value in declared.as_array().ok_or(CARD_INVALID)? {
        let mode: mime::Mime = value
            .as_str()
            .ok_or(CARD_INVALID)?
            .parse()
            .map_err(|_| CARD_INVALID)?;
        let plain = matches!(mode.essence_str(), "text/plain" | "text/*" | "*/*");
        compatible |= plain
            && mode.params().all(|(name, value)| {
                name == mime::CHARSET && value.as_str().eq_ignore_ascii_case("utf-8")
            });
    }
    if compatible {
        Ok(())
    } else {
        Err(CARD_UNSUPPORTED)
    }
}

fn card_security(body: &Value) -> Result<bool, &'static str> {
    // No skill selection or secondary auth negotiation in this text-only path.
    if body.get("security").is_some()
        || optional_array(body, "skills")
            .ok_or(SECURITY)?
            .iter()
            .any(|skill| {
                !skill.is_object()
                    || skill.get("security").is_some()
                    || optional_array(skill, "securityRequirements").is_none_or(|s| !s.is_empty())
            })
    {
        return Err(SECURITY);
    }
    let schemes = body
        .get("securitySchemes")
        .map(|s| s.as_object().ok_or(SECURITY))
        .transpose()?;
    let requirements = optional_array(body, "securityRequirements").ok_or(SECURITY)?;
    match requirements {
        [] if schemes.is_none_or(|s| s.is_empty()) => Ok(false),
        [requirement] => {
            let named = requirement
                .get("schemes")
                .and_then(Value::as_object)
                .ok_or(SECURITY)?;
            let schemes = schemes.ok_or(SECURITY)?;
            if named.len() != 1 || schemes.len() != 1 {
                return Err(SECURITY);
            }
            let (key, scopes) = named.iter().next().ok_or(SECURITY)?;
            let definition = schemes
                .get(key)
                .and_then(Value::as_object)
                .ok_or(SECURITY)?;
            if key.is_empty()
                || !scopes.is_object()
                || scopes
                    .as_object()
                    .is_some_and(|s| s.keys().any(|k| k != "list"))
                || !optional_array(scopes, "list").ok_or(SECURITY)?.is_empty()
                || definition.len() != 1
                || !definition
                    .get("httpAuthSecurityScheme")
                    .and_then(|s| s.get("scheme"))
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
            {
                return Err(SECURITY);
            }
            Ok(true)
        }
        _ => Err(SECURITY),
    }
}

fn parse_card(url: &Url, body: &Value) -> Result<Card, &'static str> {
    let name = bounded_label(&body["name"]).ok_or(CARD_INVALID)?;
    let requires_bearer = card_security(body)?;
    check_text_modes(body, "defaultInputModes")?;
    check_text_modes(body, "defaultOutputModes")?;
    if let Some(capabilities) = body.get("capabilities")
        && (!capabilities.is_object()
            || optional_array(capabilities, "extensions")
                .ok_or(CARD_INVALID)?
                .iter()
                .any(|ext| {
                    !ext.is_object()
                        || ext
                            .get("required")
                            .is_some_and(|r| r.as_bool() != Some(false))
                }))
    {
        return Err(CARD_UNSUPPORTED);
    }
    let mut interfaces = body["supportedInterfaces"]
        .as_array()
        .ok_or(CARD_UNSUPPORTED)?
        .iter()
        .filter(|i| {
            i["protocolBinding"] == "JSONRPC"
                && i["protocolVersion"] == "1.0"
                && i.get("tenant")
                    .is_none_or(|tenant| tenant.as_str() == Some(""))
        });
    let interface = interfaces.next().ok_or(CARD_UNSUPPORTED)?;
    if interfaces.next().is_some() {
        return Err(CARD_UNSUPPORTED);
    }
    let endpoint =
        Url::parse(interface["url"].as_str().ok_or(CARD_INVALID)?).map_err(|_| CARD_INVALID)?;
    if !matches!(endpoint.scheme(), "https" | "http")
        || endpoint.origin() != url.origin()
        || endpoint.cannot_be_a_base()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err("a2a_endpoint_not_allowed");
    }
    Ok(Card {
        name: name.to_owned(),
        endpoint,
        requires_bearer,
    })
}

async fn limited_body(mut response: reqwest::Response) -> Result<Vec<u8>, ()> {
    let json_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if !response.status().is_success()
        || !json_type
        || response.content_length().is_some_and(|n| n > BYTES as u64)
    {
        return Err(());
    }
    let mut result = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > BYTES - result.len() {
            return Err(());
        }
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

pub(super) async fn inspect_card(client: &Client, card: &Url) -> Result<Card, &'static str> {
    let unavailable = "a2a_card_unavailable";
    let response = client
        .get(card.clone())
        .timeout(RPC_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| unavailable)?;
    let bytes = limited_body(response).await.map_err(|_| unavailable)?;
    let value = serde_json::from_slice(&bytes).map_err(|_| CARD_INVALID)?;
    parse_card(card, &value)
}

#[derive(Debug, serde::Serialize)]
pub(super) struct Answer {
    pub text: String,
    pub context_id: String,
    pub task_id: Option<String>,
}

// Preserve explicit null separately from an absent key. The typed outer
// envelope also makes serde reject duplicate IDs/results/version keys.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct Envelope {
    jsonrpc: String,
    id: String,
    #[serde(default, deserialize_with = "present")]
    result: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    error: Option<Value>,
}

fn envelope(bytes: &[u8], id: &str) -> Result<Value, &'static str> {
    let response: Envelope = serde_json::from_slice(bytes).map_err(|_| UNKNOWN)?;
    if response.id != id || response.jsonrpc != "2.0" {
        return Err(UNKNOWN);
    }
    match (response.result, response.error) {
        (Some(v), None) if v.is_object() => Ok(v),
        (None, Some(e)) if e["code"].is_i64() && e["message"].is_string() => {
            Err("a2a_remote_rejected")
        }
        _ => Err(UNKNOWN),
    }
}

fn rpc_request(
    client: &Client,
    endpoint: &Url,
    bearer: Option<&str>,
    method: &str,
    params: Value,
    id: &str,
) -> reqwest::RequestBuilder {
    let mut request = client
        .post(endpoint.clone())
        .timeout(RPC_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("A2A-Version", "1.0")
        .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    request
}

async fn rpc(
    client: &Client,
    endpoint: &Url,
    bearer: Option<&str>,
    method: &str,
    params: Value,
) -> Result<Value, &'static str> {
    let id = uuid::Uuid::new_v4().to_string();
    let response = rpc_request(client, endpoint, bearer, method, params, &id)
        .send()
        .await
        .map_err(|_| UNKNOWN)?;
    let bytes = limited_body(response).await.map_err(|_| UNKNOWN)?;
    envelope(&bytes, &id)
}

fn text_parts(parts: &Value) -> Result<String, &'static str> {
    let mut text = String::new();
    for part in parts.as_array().ok_or(INVALID)? {
        let piece = part["text"].as_str().ok_or(INVALID)?;
        if ["data", "raw", "url", "file"]
            .iter()
            .any(|field| part.get(field).is_some())
            || piece.len() > TEXT_BYTES - text.len()
        {
            return Err(INVALID);
        }
        text.push_str(piece);
    }
    if text.trim().is_empty() {
        return Err(INVALID);
    }
    Ok(text)
}

fn task_result(task: &Value, id: &str, context: &str) -> Result<Option<String>, &'static str> {
    if task["id"] != id || task["contextId"] != context {
        return Err(UNCONFIRMED);
    }
    match task["status"]["state"].as_str() {
        Some("TASK_STATE_COMPLETED") => {
            // Only one unambiguous text artifact; never use history/status text
            // (which can simply contain the user's prompt) as a fallback.
            let [artifact] = task["artifacts"].as_array().ok_or(INVALID)?.as_slice() else {
                return Err(INVALID);
            };
            text_parts(&artifact["parts"]).map(Some)
        }
        Some("TASK_STATE_SUBMITTED" | "TASK_STATE_WORKING") => Ok(None),
        Some("TASK_STATE_AUTH_REQUIRED" | "TASK_STATE_INPUT_REQUIRED") => {
            Err("a2a_task_needs_input")
        }
        Some("TASK_STATE_FAILED" | "TASK_STATE_CANCELED" | "TASK_STATE_REJECTED") => {
            Err("a2a_task_failed")
        }
        _ => Err(INVALID),
    }
}

async fn exchange<F, Fut>(
    text: &str,
    polls: usize,
    interval: Duration,
    mut call: F,
) -> Result<Answer, &'static str>
where
    F: FnMut(&'static str, Value) -> Fut,
    Fut: Future<Output = Result<Value, &'static str>>,
{
    if text.trim().is_empty() || text.len() > TEXT_BYTES {
        return Err("a2a_invalid_input");
    }
    let deadline = tokio::time::Instant::now() + TOTAL_TIMEOUT;
    let context_id = uuid::Uuid::new_v4().to_string();
    let message_id = uuid::Uuid::new_v4().to_string();
    let result = call("SendMessage", json!({
        "message":{"role":"ROLE_USER","messageId":message_id,"contextId":context_id,"parts":[{"text":text}]},
        "configuration":{"acceptedOutputModes":["text/plain"],"returnImmediately":true,"historyLength":0}
    })).await?;
    match (result.get("message"), result.get("task")) {
        (Some(message), None) => {
            let id = bounded_label(&message["messageId"]).ok_or(INVALID)?;
            if message["role"] != "ROLE_AGENT"
                || message["contextId"] != context_id
                || id == message_id
            {
                return Err(INVALID);
            }
            let task_id = message
                .get("taskId")
                .map(|t| bounded_label(t).map(str::to_owned).ok_or(INVALID))
                .transpose()?;
            Ok(Answer {
                text: text_parts(&message["parts"])?,
                context_id,
                task_id,
            })
        }
        (None, Some(task)) => {
            let id = bounded_label(&task["id"]).ok_or(UNKNOWN)?;
            let mut answer = task_result(task, id, &context_id)?;
            for _ in 0..polls {
                if answer.is_some() {
                    break;
                }
                tokio::time::timeout_at(deadline, tokio::time::sleep(interval))
                    .await
                    .map_err(|_| "a2a_task_pending")?;
                let read = tokio::time::timeout_at(
                    deadline,
                    call("GetTask", json!({"id":id,"historyLength":0})),
                )
                .await
                .map_err(|_| UNCONFIRMED)?
                .map_err(|_| UNCONFIRMED)?;
                answer = task_result(&read, id, &context_id)?;
            }
            Ok(Answer {
                text: answer.ok_or("a2a_task_pending")?,
                context_id,
                task_id: Some(id.to_owned()),
            })
        }
        _ => Err(UNKNOWN),
    }
}

pub(super) async fn invoke(
    client: &Client,
    endpoint: &Url,
    bearer: Option<&str>,
    text: &str,
) -> Result<Answer, &'static str> {
    let answer = exchange(text, 24, POLL_INTERVAL, |method, params| {
        rpc(client, endpoint, bearer, method, params)
    })
    .await?;
    // Never pass a reflected transport credential on to the API caller.
    if bearer.is_some_and(|s| {
        answer.text.contains(s) || answer.task_id.as_deref().is_some_and(|id| id.contains(s))
    }) {
        return Err(INVALID);
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn location() -> Url {
        Url::parse("https://agent.example/.well-known/agent-card.json").unwrap()
    }

    fn card() -> Value {
        json!({
            "name": "Counting agent", "description":"Fixture", "version":"1",
            "supportedInterfaces":[
                {"protocolBinding":"GRPC","protocolVersion":"1.0","url":"https://agent.example/grpc"},
                {"protocolBinding":"JSONRPC","protocolVersion":"1.0","url":"https://agent.example/rpc"}
            ],
            "defaultInputModes":["text/plain"],"defaultOutputModes":["text/plain"],
            "capabilities":{}, "skills":[]
        })
    }

    #[test]
    fn selects_compatible_same_origin_interface_without_inferring_auth() {
        let parsed = parse_card(&location(), &card()).unwrap();
        assert_eq!(parsed.endpoint.as_str(), "https://agent.example/rpc");
        assert_eq!(parsed.name, "Counting agent");
        assert!(!parsed.requires_bearer);
    }

    #[test]
    fn requires_declared_input_and_output_modes_to_allow_plain_text() {
        for field in ["defaultInputModes", "defaultOutputModes"] {
            for modes in [
                json!(["application/json", "text/html"]),
                json!(["text/plain+json"]),
                json!(["text/plain; charset=iso-8859-1"]),
                json!([]),
            ] {
                let mut c = card();
                c[field] = modes;
                assert_eq!(
                    parse_card(&location(), &c).unwrap_err(),
                    CARD_UNSUPPORTED,
                    "{field}"
                );
            }
            for malformed in [Value::Null, json!("text/plain"), json!(["text/plain", 42])] {
                let mut c = card();
                c[field] = malformed;
                assert_eq!(
                    parse_card(&location(), &c).unwrap_err(),
                    CARD_INVALID,
                    "{field}"
                );
            }
            for supported in [
                "text/plain",
                "TEXT/PLAIN",
                "text/plain; charset=UTF-8",
                "text/*",
                "*/*",
            ] {
                let mut c = card();
                c[field] = json!(["application/json", supported]);
                assert!(parse_card(&location(), &c).is_ok(), "{field}: {supported}");
            }
            let mut c = card();
            c.as_object_mut().unwrap().remove(field);
            assert!(parse_card(&location(), &c).is_ok(), "omitted {field}");
        }
    }

    #[test]
    fn two_pending_tasks_fit_the_fixtures_shared_rpc_budget() {
        // Two sends at t=0, then two GetTask reads on every tick in [0, 60s).
        let mut calls = 2;
        let mut elapsed = POLL_INTERVAL;
        while elapsed < Duration::from_secs(60) {
            calls += 2;
            elapsed += POLL_INTERVAL;
        }
        assert!(
            calls <= 30,
            "two pending tasks would make {calls} RPCs in one minute"
        );
    }

    #[test]
    fn accepts_explicit_unambiguous_a2a_bearer_requirement() {
        let mut c = card();
        c["securitySchemes"] = json!({"peer":{"httpAuthSecurityScheme":{"scheme":"Bearer"}}});
        c["securityRequirements"] = json!([{"schemes":{"peer":{"list":[]}}}]);
        assert!(parse_card(&location(), &c).unwrap().requires_bearer);
    }

    #[test]
    fn rejects_unnegotiated_interface_or_security_instead_of_guessing() {
        for (field, value) in [
            ("tenant", json!("team")),
            ("protocolVersion", json!("0.3")),
            ("url", json!("https://different.example/rpc")),
            ("url", json!("https://agent.example/rpc?key=x")),
        ] {
            let mut c = card();
            c["supportedInterfaces"][1][field] = value;
            assert!(parse_card(&location(), &c).is_err(), "{field}");
        }
        for (field, value) in [
            ("security", json!([{"legacy":[]}])),
            ("securityRequirements", json!([{"schemes":{"unknown":{}}}])),
            ("securityRequirements", json!([{},{"schemes":{"other":{}}}])),
            (
                "skills",
                json!([{"securityRequirements":[{"schemes":{"other":{}}}]}]),
            ),
            (
                "capabilities",
                json!({"extensions":[{"uri":"urn:required","required":true}]}),
            ),
        ] {
            let mut c = card();
            c[field] = value;
            assert!(parse_card(&location(), &c).is_err(), "{field}");
        }
    }

    #[test]
    fn correlates_jsonrpc_and_never_returns_remote_errors_as_answers() {
        assert_eq!(
            envelope(br#"{"id":"one","jsonrpc":"2.0","result":{"a":3}}"#, "one"),
            Ok(json!({"a":3}))
        );
        for bytes in [
            r#"{"id":"two","jsonrpc":"2.0","result":{}}"#,
            r#"{"id":"two","id":"one","jsonrpc":"2.0","result":{}}"#,
            r#"{"id":"one","jsonrpc":"1.0","result":{}}"#,
            r#"{"id":"one","jsonrpc":"2.0","result":{},"error":null}"#,
            r#"{"id":"one","jsonrpc":"2.0","error":{"code":-1,"message":"private detail"}}"#,
        ] {
            assert!(
                !envelope(bytes.as_bytes(), "one")
                    .unwrap_err()
                    .contains("private detail")
            );
        }
    }

    #[tokio::test]
    async fn rejects_non_json_responses_and_caps_bytes_without_declared_length() {
        fn chunks(mime: &str, pieces: Vec<Vec<u8>>) -> reqwest::Response {
            let stream = futures::stream::iter(pieces.into_iter().map(Ok::<_, std::io::Error>));
            axum::http::Response::builder()
                .header("Content-Type", mime)
                .body(reqwest::Body::wrap_stream(stream))
                .unwrap()
                .into()
        }
        assert!(
            limited_body(chunks("text/html", vec![b"{}".to_vec()]))
                .await
                .is_err()
        );
        assert_eq!(
            limited_body(chunks(
                "application/json; charset=utf-8",
                vec![b"{".to_vec(), b"}".to_vec()]
            ))
            .await
            .unwrap(),
            b"{}"
        );
        assert!(
            limited_body(chunks("application/json", vec![vec![0; BYTES], vec![1]]))
                .await
                .is_err()
        );
    }

    #[test]
    fn sends_only_protocol_headers_and_explicit_peer_credentials() {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let url = Url::parse("https://agent.example/rpc").unwrap();
        for token in [None, Some("dedicated-peer-token")] {
            let request = rpc_request(
                &client,
                &url,
                token,
                "SendMessage",
                json!({"message":{"parts":[{"text":"17?"}]}}),
                "abc",
            )
            .build()
            .unwrap();
            assert_eq!(request.url(), &url);
            assert_eq!(request.headers()["a2a-version"], "1.0");
            assert_eq!(
                request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok()),
                token.map(|_| "Bearer dedicated-peer-token")
            );
            assert!(request.headers().get("cookie").is_none());
            let payload: Value =
                serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
            assert_eq!(
                payload,
                json!({"jsonrpc":"2.0","id":"abc","method":"SendMessage","params":{"message":{"parts":[{"text":"17?"}]}}})
            );
        }
    }

    #[tokio::test]
    async fn returns_actual_agent_text_only_from_the_submitted_context() {
        for (role, same, expected) in [
            ("ROLE_AGENT", true, Some("seventeen")),
            ("ROLE_USER", true, None),
            ("ROLE_AGENT", false, None),
        ] {
            let reply = exchange("add eight and nine", 1, Duration::ZERO, |method, params| {
                assert_eq!(method, "SendMessage");
                let msg = &params["message"];
                assert_eq!(msg["role"], "ROLE_USER");
                assert_eq!(msg["parts"], json!([{"text":"add eight and nine"}]));
                for field in ["contextId", "messageId"] {
                    assert!(uuid::Uuid::parse_str(msg[field].as_str().unwrap()).is_ok());
                }
                assert_ne!(msg["contextId"], msg["messageId"]);
                let context = if same {
                    &msg["contextId"]
                } else {
                    &Value::Null
                };
                std::future::ready(Ok(
                    json!({"message":{"role":role,"messageId":"answer-1","contextId":context,
                    "parts":[{"text":"seven"},{"text":"teen"}]}}),
                ))
            })
            .await;
            assert_eq!(
                reply.as_ref().ok().map(|answer| answer.text.as_str()),
                expected
            );
            if let Ok(answer) = reply {
                assert!(answer.task_id.is_none());
                assert!(!answer.context_id.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn submits_once_and_polls_only_the_acknowledged_task() {
        let mut calls = Vec::new();
        let mut context = Value::Null;
        let answer = exchange("probe", 2, Duration::ZERO, |method, params| {
            calls.push(method);
            std::future::ready(Ok(if method == "SendMessage" {
                context = params["message"]["contextId"].clone();
                json!({"task":{"id":"t-1","contextId":context,"status":{"state":"TASK_STATE_SUBMITTED"}}})
            } else {
                assert_eq!(params["id"], "t-1");
                json!({"id":"t-1","contextId":context,"status":{"state":"TASK_STATE_COMPLETED"},
                    "artifacts":[{"artifactId":"a-1","parts":[{"text":"17"}]}]})
            }))
        }).await.unwrap();
        assert_eq!(calls, ["SendMessage", "GetTask"]);
        assert_eq!(answer.text, "17");
        assert_eq!(answer.task_id.as_deref(), Some("t-1"));
        assert_eq!(context, answer.context_id);
    }

    #[tokio::test]
    async fn never_resubmits_on_unknown_outcome_and_bounds_pending_work() {
        let mut sends = 0;
        let unknown = exchange("probe", 2, Duration::ZERO, |_, _| {
            sends += 1;
            std::future::ready(Err("a2a_submission_unknown"))
        })
        .await;
        assert_eq!(unknown.unwrap_err(), "a2a_submission_unknown");
        assert_eq!(sends, 1);
        let mut calls = Vec::new();
        let mut task = Value::Null;
        let pending = exchange("probe", 2, Duration::ZERO, |method, p| {
            calls.push(method);
            std::future::ready(Ok(if method=="SendMessage" {
                task=json!({"id":"t","contextId":p["message"]["contextId"],"status":{"state":"TASK_STATE_WORKING"}});
                json!({"task":task})
            } else {task.clone()}))
        }).await;
        assert_eq!(pending.unwrap_err(), "a2a_task_pending");
        assert_eq!(calls, ["SendMessage", "GetTask", "GetTask"]);
    }

    #[tokio::test]
    async fn rejects_mixed_parts_wrong_poll_identity_and_non_success_states() {
        for variant in [
            "mixed",
            "wrong-id",
            "wrong-context",
            "failed",
            "no-artifact",
            "ambiguous",
        ] {
            let mut context = Value::Null;
            let answer = exchange("probe",1,Duration::ZERO, |method, params| {
                std::future::ready(Ok(if method=="SendMessage" {
                    context = params["message"]["contextId"].clone();
                    let mut r=json!({"task":{"id":"t","contextId":context,"status":{"state":"TASK_STATE_WORKING"}}});
                    if variant=="ambiguous" {r["message"]=json!({});} r
                } else {
                    let mut t=json!({"id":"t","contextId":context,"status":{"state":"TASK_STATE_COMPLETED"},
                        "artifacts":[{"artifactId":"a","parts":[{"text":"17"}]}]});
                    match variant {
                        "mixed"=> t["artifacts"][0]["parts"][0]["data"]=json!(5),
                        "wrong-id"=>t["id"]=json!("other"),
                        "wrong-context"=>t["contextId"]=json!("other"),
                        "failed"=>t["status"]["state"]=json!("TASK_STATE_FAILED"),
                        "no-artifact"=>t["artifacts"]=json!([]),
                        _=>{}
                    }
                    t
                }))
            }).await;
            assert!(answer.is_err(), "{variant}");
        }
    }
}
