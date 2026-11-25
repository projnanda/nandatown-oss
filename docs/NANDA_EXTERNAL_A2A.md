# Connect an independently running A2A agent to Town

Use **Agents → Connect external A2A agent** (or open `/nanda/external-agents` on your Town server) to try a text task without uploading the agent, moving its model credentials, or changing who runs it. This is a private, operator-enabled NANDA integration. The agent stays on its own host; disconnecting does not stop it.

## Enable a destination

The Town operator sets `NANDA_EXTERNAL_A2A_TARGETS` on the **Oasis server** (in the server's `.env` for Compose), then restarts or recreates it. Approve both the public card URL and the exact invocation URL. URLs containing a username, password, query string or fragment are not accepted.

```sh
export NANDA_EXTERNAL_A2A_TARGETS='[{"card_url":"https://your-agent.example/.well-known/agent-card.json","endpoint":"https://your-agent.example/a2a/v1"}]'
```

Without a configured destination, new previews, registrations and calls are disabled. Existing owners can still list and disconnect their connections. For a real deployment the endpoints must use HTTPS on the same origin, with public addresses. DNS answers are checked and pinned for each operation. The client does not follow redirects or use a system HTTP proxy. Start with independently operated agents whose operators consent to receiving test traffic.

## Use it

1. Sign in, open the external-agent page, enter the card URL, and preview it. Check the agent name and **entire invocation URL**. Preview fetches only the public card; it sends no credential or task.
2. Choose anonymous access or enter a bearer issued specifically for this peer. Some cards omit auth information even though their agents require a token. Never use your Town session token, the agent's admin token, or a model-provider API key. Town encrypts the peer token and never shows it in the catalog.
3. Connect, select the connection, and send one text task. The answer comes back to the Oasis caller. For a task response, the page also shows the remote task ID and context ID; for an immediate agent message there is no remote task ID. A listing or HTTP acknowledgement alone says nothing about task success.
4. Disconnect to remove the binding and its stored peer credential. Revoke that credential at the independently operated agent if appropriate. To change a connection's endpoint or credential, disconnect and register again.

**Supported in this version:** a non-tenant A2A 1.0 JSON-RPC interface; explicit anonymous or dedicated bearer authentication; one text message or one completed text artifact. Pending tasks are read with `GetTask`. No streaming, OAuth negotiation, automatic resubmission, or scheduling. If the call times out or the return path breaks, the agent may already have done the work. Check with its operator before sending it again.

This is a bounded interactive call, not an asynchronous job store. At most two outbound operations can run per server process. On timeout or a lost reply, this version does not provide a resumable task handle; correlate the time and task with the peer operator. The ordinary catalog detail identifies the private agent and links its native task path in `metadata.nanda_external`; its generic URL is not a forwarding endpoint.

### Local proof with OpenClaw, no model account

Start the [isolated offline OpenClaw fixture](../scripts/oasis-external-openclaw/README.md#persistent-local-agent-and-relay). On the same machine, run the **native debug** Oasis server with the following configuration and a dedicated local test user:

```sh
export NANDA_EXTERNAL_A2A_TARGETS='[{"card_url":"http://127.0.0.1:18916/.well-known/agent-card.json","endpoint":"http://127.0.0.1:18916/a2a/v1"}]'
```

This exact pair is an opt-in exception for the offline fixture in debug builds. It is not a tunnel or a production setting. A hosted or containerized Oasis cannot use the host's loopback address to reach this fixture.

With `OASIS_TEST_USERNAME` and `OASIS_TEST_PASSWORD` set privately for that disposable account, and Docker pointed at the fixture's engine:

```sh
# From the repository root. Change the port only if your local server uses another one.
OASIS_TEST_PORT=8080 node scripts/oasis-external-openclaw/oasis-proof.mjs
```

The check logs in, previews the card, creates a new private connection, asks for the sum of 8 and 9 **through Oasis**, and requires `17`. It then reads the same task and context directly from OpenClaw and disconnects the test connection. It refuses to replace a pre-existing connection. The model is a local test double; the A2A agent, Oasis server, authenticated request, storage and return path are real.

## API and scope

All routes are relative to `/api/nanda/external-agents` and require a regular signed-in owner. JSON mutations use `Content-Type: application/json`.

| Action | Request | Result |
| --- | --- | --- |
| Preview | `POST /preview` with `card_url` | Name, full endpoint and whether the card declares required bearer auth |
| Register | `POST` with `card_url`, `expected_endpoint`, `auth_mode` and optional `peer_token` | New private connection and catalog ID (`201`); duplicate registration conflicts |
| List | `GET` | This owner's connections, without credentials |
| Invoke | `POST /{id}/tasks` with `text` | `text`, `context_id`, and nullable `task_id` |
| Disconnect | `DELETE /{id}` | `204` |

The generic managed-agent proxy is not changed into an external URL forwarder. A card cannot reroute the outside agent's own model calls or grant MCP access. For the reverse direction, see the [agent-side model and tool gateway instructions](../scripts/oasis-external-openclaw/README.md#if-the-external-agent-wants-to-use-the-oasis-model-or-tool-gateway). Registering an agent does not prove model spend or token savings. This integration does not publish an OpenTelemetry collector.

If the peer also needs to export its own spans, request a tenant-specific, authenticated OTLP ingestion URL from the hosting operator. Do not point it at internal collector ports or reuse the agent's A2A bearer as a telemetry credential. Public intake, tenant attribution, redaction, rate limits and retention are separate hosting work; sending a task does not depend on them.
