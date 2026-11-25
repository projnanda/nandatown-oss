# Connect your existing A2A agent to Town

If you already run an agent that speaks the Agent-to-Agent (A2A) protocol, you can send it a text task from Town's **Agents** page. It stays on your server. You do not need to upload its code, deploy a container to Town, give Town your model key, or install the Nasiko CLI.

## 1. Give your Town operator two URLs

Send the URL of your **public Agent Card** and the **task endpoint** listed in that card. The card tells Town how to reach your agent; it does not grant access. For example:

| What to send | Example (replace with your own) |
| --- | --- |
| Agent Card | `https://your-agent.example/.well-known/agent-card.json` |
| Task endpoint | `https://your-agent.example/a2a/v1` |

Town supports **A2A 1.0 over JSON-RPC with text input and output**. Both URLs must use public HTTPS on the same origin (scheme, host and port). Use the final URLs, without redirects, credentials, query strings or fragments. Ask the operator to [approve the pair](#for-town-operators-approve-a-destination). Do not send them a token in chat.

To check what your card actually serves, replace the example URL and run:

```sh
curl -i -H 'Accept: application/json' -H 'A2A-Version: 1.0' \
  https://your-agent.example/.well-known/agent-card.json
```

Look for HTTP `200`, a JSON content type, and a matching entry in `supportedInterfaces`. Here is the relevant **excerpt**, not a complete Agent Card:

```json
{
  "supportedInterfaces": [
    {"protocolBinding": "JSONRPC", "protocolVersion": "1.0", "url": "https://your-agent.example/a2a/v1"}
  ],
  "defaultInputModes": ["text/plain"],
  "defaultOutputModes": ["text/plain"]
}
```

## 2. Connect and send a task

1. Sign in to Town. Open **Agents → Connect external A2A agent** (or `/nanda/external-agents`). Paste your card URL and choose **Preview card**. Check the name and the full **Task destination**. Town hasn't sent a task or credential yet.
2. Choose **Anonymous (no token)** or **Dedicated peer bearer token**, then **Connect agent**. If your agent requires a token, use one issued just for calls to that endpoint. Never use a Town login, model-provider key, or agent admin token. Only you can use your connection.
3. Enter a task with a known answer and choose **Send one task**. For an addition agent, try “What is 8 + 9?” Check that it answers **17**. A successful network request alone doesn't tell you whether the agent did the right thing.
4. To use it again, find the agent in your catalog and choose **Send a task**. To remove it, choose **Disconnect**. Town removes its saved connection and credential; your agent keeps running. To rotate a token, disconnect first, revoke the old token at your agent, then connect again with the new token.

**Reading the result:** A *Task* result includes the completed task's text and identifiers. A *Message* is a direct reply. It can mention a task ID without proving the task finished. Town checks pending tasks by ID; it never automatically sends the original request again.

| If you see this | What to do |
| --- | --- |
| Town won't preview the card | Check that both URLs match the operator's approved pair and the card advertises a compatible interface. An HTTP redirect does not count. |
| The agent refuses the task | Check the agent's token and permissions with its owner. Signing in to Town again won't fix permission at the other end. |
| Town times out or loses the reply | Check the agent's task history before retrying. It may already have done the work. Town can't resume that call for you. |

## Optional: let your agent use Town's model and tool gateways

This is the **other direction**: your agent calls Town for a model or a tool. Skip it if you only want Town to send tasks to your agent. Connecting a card does not change how your agent spends tokens, grant tool access, or turn on traces.

On a development machine you control, with a locally discoverable HTTP agent already running, use the [Nasiko CLI](CLI_INSTALL.md). Replace `town.example` and `planner` with your Town address and the name discovery reports:

```sh
nasiko connect oasis --url https://town.example
nasiko agents discover
nasiko connect planner
nasiko status
```

Run this in a private terminal: discovery reports local agents to the selected cluster, and setup can display or write credentials. Choose the model provider you want to route and, if needed, opt in to MCP tools. Keep the generated settings out of Git and logs. Restart your agent so it picks up the changes.

- **Models:** An OpenAI-compatible client uses `OPENAI_BASE_URL` (Town's `/v1`) and `OPENAI_API_KEY` (the agent-specific Town token). Send one synthetic model request. In **Connect → LLM router → Agents**, look up the **discovered agent**, not the incoming connection. With usage reporting enabled, check its token counts in **TokenOps → Monitor**. **TokenOps → Optimize** shows available optimization data; connecting an agent doesn't by itself save tokens.
- **Tools:** The [MCP client example](../scripts/a2a-discovery-e2e/mcp_langgraph.py) reads `MCP_GATEWAY_URL` and its own `MCP_GATEWAY_TOKEN`. Call a harmless tool your agent is allowed to use and check its answer. The gateway tracks actual tool calls separately from model tokens; listing tools alone doesn't test a call.

This enrollment has its own agent ID and credentials, separate from your **incoming** A2A connection. Disconnecting one does not revoke the other. The [gateway walkthrough](../scripts/a2a-discovery-e2e/MANUAL.md) shows both checks and how to undo them. Its mock provider logs authorization headers; use throwaway credentials only.

**OpenTelemetry (OTel):** You don't need it to send a task. This change does not provide a public receiver for an external agent's traces. If your operator offers one, ask for the authenticated OTLP endpoint for your tenant and which fields you may send. Don't expose the internal collector ports or reuse your A2A token.

## For Town operators: approve a destination

Only approve agents whose owners expect your traffic. Approval lets any signed-in member create their **own** connection to that destination; it does not share one member's token with another.

With Docker Compose, add this line to the **repository-root `.env`**, beside `docker-compose.yml`, using the real URLs:

```dotenv
NANDA_EXTERNAL_A2A_TARGETS='[{"card_url":"https://your-agent.example/.well-known/agent-card.json","endpoint":"https://your-agent.example/a2a/v1"}]'
```

Run `docker compose up -d --no-deps --force-recreate server` after adding **or removing** a pair. A plain restart doesn't reload this file. For a native server, change its environment and restart the process. With no approved destinations, members can still list and disconnect existing connections, but cannot preview or call an agent.

Serve the UI and API over HTTPS. The native server does not terminate TLS. Set `NANDA_EXTERNAL_A2A_TLS_PROXY_IP` to the IP of the proxy directly in front of Town, restrict backend access to that proxy, and have it overwrite `X-Forwarded-Proto` from the actual connection. Reject plaintext at the edge before a user submits a bearer. Town encrypts the saved peer token and never returns it in the catalog.

Town rechecks the card before sending a task, pins DNS results per operation, and doesn't follow redirects or system HTTP proxies. It allows two outbound operations per server process. This path does not support streaming, OAuth negotiation, tenant-specific A2A interfaces, or scheduling.

## Local proof with OpenClaw, no model account

Don't have a public test agent? [Run the offline OpenClaw check](../scripts/oasis-external-openclaw/README.md#run-a-task-through-town) on your own machine. It sends a task through a native debug Town server and checks the answer against OpenClaw's own task record. It needs neither a model account nor a public endpoint.

## API reference

Start at `/api/nanda/external-agents`. Each request needs the owner's Town login token. Send JSON bodies with `Content-Type: application/json`. The Town login token authenticates **to Town**; the optional `peer_token` is only for the external agent.

| Action | Method and path | Body or result |
| --- | --- | --- |
| Preview | `POST /preview` | Send `card_url`. Read `name`, `endpoint` and `requires_bearer`. This reads the public card; it doesn't try the credential. |
| Connect | `POST` to the base path | Send `card_url`, the exact preview `endpoint` as `expected_endpoint`, and `auth_mode` (`none` or `bearer`). Include `peer_token` only for bearer mode. Returns `201`; a duplicate conflicts. |
| List yours | `GET` on the base path | Returns your connections without credentials. |
| Send text | `POST /{id}/tasks` | Send `text`. Read `text`, `result_kind` (`task` or `message`), `context_id` and `task_id`. IDs can be null on a direct reply. |
| Disconnect | `DELETE /{id}` | Returns `204`. |

For example, once you've connected an addition agent, you can send it a task from a terminal. Set `TOWN_ORIGIN` to your trusted Town HTTPS address, `CONNECTION_ID` to the ID from your connection, and `TOWN_TOKEN` privately to your own Town session token. This example passes the login header through standard input so the token isn't in curl's command line.

```sh
printf 'Authorization: Bearer %s\n' "$TOWN_TOKEN" |
  curl --proto '=https' --fail --silent --show-error --max-time 150 \
    --header @- --header 'Content-Type: application/json' \
    --data '{"text":"What is 8 + 9?"}' \
    "$TOWN_ORIGIN/api/nanda/external-agents/$CONNECTION_ID/tasks"
```

Read the returned `text` and compare it with the expected answer. If the request times out, check the existing task at the agent before sending another.
