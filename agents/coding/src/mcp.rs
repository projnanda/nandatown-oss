//! mcp.rs — the MCP-gateway client for platform agents.
//!
//! The whole configuration is two env vars, both injected at deploy time
//! (docs: `oss/docs/MCP_GATEWAY_DESIGN.md`, `docs/MCP_GATEWAY_AGENT_AUTH.md`):
//!
//!   `MCP_GATEWAY_URL`   — where the platform's aggregating gateway lives
//!   `MCP_GATEWAY_TOKEN` — this agent's own credential, sent as a Bearer header
//!
//! There is NO per-request credential to plumb: the platform resolves the
//! calling user server-side from the `traceparent` this agent forwards, and
//! authorizes the call only when this agent is a recorded participant of that
//! flow. An agent that drops the header gets a clean 403, never misattribution.
//!
//! # Why the tool list is per request, not per process
//!
//! `tools/list` is **query-aware** (`docs/MCP_TOOL_SEARCH_DESIGN.md`): the
//! gateway resolves the user's query from the flow named by `traceparent` and
//! returns only the tools matching it, plus `nasiko_search_tools`. Called
//! WITHOUT a traceparent — as a startup probe would — it returns the search
//! meta-tool and nothing else.
//!
//! So an agent that discovers once at startup advertises exactly one tool
//! forever. The model then searches, gets a tool name back, cannot call it
//! (it was never advertised), and searches again until its round budget dies.
//! Hence [`McpSession`]: one per A2A request, seeded from a `tools/list` that
//! carries that request's traceparent, and **extended in place** whenever the
//! model searches — a `nasiko_search_tools` result carries full definitions, so
//! what it finds becomes immediately callable.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{Value, json};

/// The gateway's tool-search meta-tool. Always present in `tools/list`; its
/// result carries the definitions of whatever it found.
const SEARCH_TOOL: &str = "nasiko_search_tools";

/// Thin JSON-RPC client over the platform's MCP gateway.
///
/// Holds no tool state — see [`McpSession`], which owns the per-request set.
pub struct McpClient {
    url: String,
    token: String,
    http: reqwest::Client,
}

impl McpClient {
    /// Returns a configured client, or `None` when the platform didn't inject
    /// the gateway env (local runs, MCP disabled) — the agent then simply runs
    /// with its built-in tools only.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("MCP_GATEWAY_URL")
            .ok()
            .filter(|u| !u.is_empty());
        let token = std::env::var("MCP_GATEWAY_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        match (url, token) {
            (Some(url), Some(token)) => Some(Self {
                url,
                token,
                http: reqwest::Client::new(),
            }),
            _ => {
                // Logged rather than silent: "MCP is off" and "the image has no
                // MCP client" otherwise look identical in the agent's output.
                tracing::info!(
                    "mcp: MCP_GATEWAY_URL/MCP_GATEWAY_TOKEN not set — running with built-in tools only"
                );
                None
            }
        }
    }

    /// POSTs one JSON-RPC request to the gateway. `traceparent` may be empty
    /// (the startup probe); when set it names the flow being served and is what
    /// makes `tools/list` query-aware.
    async fn rpc(&self, traceparent: &str, body: Value) -> Result<Value, String> {
        let mut req = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&body);
        if !traceparent.is_empty() {
            req = req.header("traceparent", traceparent);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let raw = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("gateway HTTP {}: {raw}", status.as_u16()));
        }

        let parsed: Value =
            serde_json::from_str(&raw).map_err(|e| format!("gateway response parse: {e}"))?;
        if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or_default();
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            // `data` carries the structured half of a gateway refusal: for an
            // AUTH_REQUIRED pause, the connector and the recorded HITL request
            // id. Keeping only the prose throws that away.
            let data = err
                .get("data")
                .filter(|d| !d.is_null())
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            return Err(format!("gateway error {code}: {message}{data}"));
        }
        Ok(parsed.get("result").cloned().unwrap_or_else(|| json!({})))
    }

    /// Confirms the credential works and reports what the gateway offers before
    /// any flow exists. Best-effort: a failure only means the agent starts
    /// without knowing, not that requests will fail.
    ///
    /// This is a health check, NOT discovery — without a traceparent the
    /// gateway returns only the search meta-tool, so the real tool set is
    /// fetched per request by [`McpSession::open`].
    pub async fn probe(&self) {
        match self
            .rpc(
                "",
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .await
        {
            Ok(result) => {
                let count = result
                    .get("tools")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                tracing::info!(count, "mcp: gateway reachable; tools resolved per request");
            }
            Err(e) => {
                tracing::warn!(error = %e, "mcp: gateway unreachable at startup, continuing");
            }
        }
    }
}

/// The gateway tools for ONE A2A request.
///
/// Separate from [`McpClient`] because the set is query-scoped and mutable: it
/// is seeded from a `tools/list` carrying this request's traceparent, and grows
/// whenever the model calls `nasiko_search_tools`.
pub struct McpSession {
    client: Arc<McpClient>,
    traceparent: String,
    /// Advertised to the model, in the OpenAI function shape.
    defs: Vec<Value>,
    names: HashSet<String>,
}

impl McpSession {
    /// Opens a session and fetches the tools matching this request's query.
    pub async fn open(client: Arc<McpClient>, traceparent: &str) -> Self {
        let mut session = Self {
            client,
            traceparent: traceparent.to_string(),
            defs: Vec::new(),
            names: HashSet::new(),
        };
        match session
            .client
            .rpc(
                &session.traceparent,
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .await
        {
            Ok(result) => {
                let added = session.absorb(&result);
                tracing::debug!(added, "mcp: tools/list for this request");
            }
            Err(e) => {
                tracing::warn!(error = %e, "mcp: tools/list failed, continuing without gateway tools");
            }
        }
        session
    }

    /// The gateway tools to advertise alongside the agent's built-ins.
    pub fn tool_defs(&self) -> &[Value] {
        &self.defs
    }

    /// Whether `name` is a gateway tool rather than one of the built-ins.
    pub fn handles(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Adds every `{name, description, inputSchema}` entry under `tools` to the
    /// advertised set, converting to the OpenAI function shape. Returns how many
    /// were new. Shared by `tools/list` and `nasiko_search_tools`, which the
    /// gateway returns in the same shape.
    fn absorb(&mut self, result: &Value) -> usize {
        let Some(tools) = result.get("tools").and_then(Value::as_array) else {
            return 0;
        };
        let mut added = 0;
        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !self.names.insert(name.to_string()) {
                continue;
            }
            // A tool may omit inputSchema when it takes no arguments, but the
            // chat-completions API rejects a function without one.
            let parameters = tool
                .get("inputSchema")
                .filter(|s| s.is_object())
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            self.defs.push(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": parameters,
                }
            }));
            added += 1;
        }
        added
    }

    /// Executes one gateway tool on behalf of this request's flow.
    ///
    /// Takes `&mut self` because a `nasiko_search_tools` result is absorbed into
    /// the advertised set: without that the model can search, be told a tool
    /// exists, and still have no way to call it. Errors come back as content
    /// rather than `Err` so the model can recover — the same convention the
    /// built-in tools follow.
    pub async fn call_tool(&mut self, name: &str, args_json: &str) -> String {
        let arguments: Value = match serde_json::from_str(args_json) {
            Ok(args) => args,
            Err(e) => return format!("bad arguments: {e}"),
        };
        let result = match self
            .client
            .rpc(
                &self.traceparent,
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": name, "arguments": arguments},
                }),
            )
            .await
        {
            Ok(result) => result,
            Err(e) => return format!("tool {name} failed: {e}"),
        };

        if name == SEARCH_TOOL {
            let before = self.defs.len();
            self.absorb(&result);
            let found: Vec<&str> = self.defs[before..]
                .iter()
                .filter_map(|d| d["function"]["name"].as_str())
                .collect();
            return if found.is_empty() {
                "No matching tools were found. Do not search again for the same thing; \
                 answer from what you already have."
                    .to_string()
            } else {
                format!(
                    "These tools are now available and can be called directly: {}. \
                     Call the one you need — do not search again.",
                    found.join(", ")
                )
            };
        }

        // Prefer the standard MCP text content; fall back to the raw result JSON
        // so structured-only results still reach the model.
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|part| {
                part.get("type").and_then(Value::as_str) == Some("text")
                    && part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| !t.is_empty())
            })
            .and_then(|part| part.get("text").and_then(Value::as_str));
        match text {
            Some(text) => text.to_string(),
            None => result.to_string(),
        }
    }

    /// A system-prompt fragment describing the connected-account tools, for the
    /// agent to append to its own prompt.
    ///
    /// Advertising the tools is not enough: an agent's prompt enumerates its
    /// built-ins and names one of them as the general-purpose fallback, so the
    /// model answers "my issues"/"my emails" from a web search instead of the
    /// user's actual account. The search→call flow also has to be spelled out,
    /// or the model treats `nasiko_search_tools` as the answer rather than the
    /// lookup. Empty when the gateway offered nothing.
    pub fn system_prompt_hint(&self) -> String {
        if self.defs.is_empty() {
            return String::new();
        }
        let direct: Vec<&str> = self
            .defs
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .filter(|name| *name != SEARCH_TOOL)
            .collect();
        let available = if direct.is_empty() {
            format!(
                "Right now only `{SEARCH_TOOL}` is listed; call it first to find the tool you need."
            )
        } else {
            format!(
                "Already available: {}. Use `{SEARCH_TOOL}` only if none of those fit.",
                direct.join(", ")
            )
        };
        format!(
            "\n\nConnected-account tools (MCP gateway). These reach the user's OWN \
             connected accounts — email, chat, issue tracker, calendar and so on — and \
             return their real data. {available}\n\
             - For any question about the user's own data (\"my issues\", \"my emails\", \
             \"messages I received\"), you MUST use these tools.\n\
             - NEVER answer such a question with a web search: that returns public \
             documentation about the product, not the user's data.\n\
             - After `{SEARCH_TOOL}` returns a tool, CALL THAT TOOL. Never search twice \
             for the same capability, and never search for a tool name you already have.\n\
             - If a tool reports the account is not connected or needs re-authentication, \
             say so plainly and stop — the platform records the request for the user to act \
             on. Pass on an authorization link only when the result actually contains one; \
             never invent or guess one."
        )
    }
}
