# Connect an independently running A2A agent to Town

Use **Agents → Connect external A2A agent** (or open `/nanda/external-agents` on your Town server) to try a text task without uploading the agent, moving its model credentials, or changing who runs it. This is a private, operator-enabled NANDA integration. The agent stays on its own host; disconnecting does not stop it.

## Enable a destination

The Town operator approves a **destination**, not an individual caller: any signed-in workspace member may make their own private connection to an approved pair. Approve both the public card URL and its exact invocation URL. URLs containing a username, password, query string or fragment are not accepted.

```sh
export NANDA_EXTERNAL_A2A_TARGETS='[{"card_url":"https://your-agent.example/.well-known/agent-card.json","endpoint":"https://your-agent.example/a2a/v1"}]'
```

Without a configured destination, new previews, registrations and calls are disabled. Existing owners can still list and disconnect their connections. For a real deployment the endpoints must use HTTPS on the same origin, with public addresses. DNS answers are checked and pinned for each operation. The client does not follow redirects or use a system HTTP proxy. Start with independently operated agents whose operators consent to receiving test traffic.

For Compose, put the setting in the server's `.env`, then run `docker compose up -d --no-deps --force-recreate server` after adding **or removing** a pair. A [plain Compose restart](https://docs.docker.com/reference/cli/docker/compose/restart/) does not reload that file. For a native process, restart it with the changed environment.

**Receiving peer credentials:** serve the operator UI and API over HTTPS. Set `NANDA_EXTERNAL_A2A_TLS_PROXY_IP` to the exact immediate reverse proxy IP, restrict access to the backend to that proxy, and have the proxy overwrite `X-Forwarded-Proto` from the actual inbound connection. Reject plaintext at the edge before it receives a bearer. The native server does not terminate TLS. The only plaintext exception is direct debug use on loopback.

## Use it

1. Sign in, open the external-agent page, enter the card URL, and preview it. Check the agent name and **entire invocation URL**. Preview fetches only the public card; it sends no credential or task.
2. Choose anonymous access or enter a bearer issued specifically for this peer. Some cards omit auth information even though their agents require a token. Town refuses JWTs signed with its current login or agent-runtime keys and the reserved formats for its gateway and state tokens. It cannot identify arbitrary provider keys or other opaque credentials: never enter a model-provider key or the agent's admin token. Town encrypts the peer token and never shows it in the catalog.
3. Connect and send one text task. To return later, open the agent in the regular catalog and choose **Send a task**. A **Task** result confirms a completed text artifact and shows the [peer-assigned context](https://a2a-protocol.org/v1.0.0/specification/#341-context-identifier-semantics) and task ID. A **Message** is a direct reply: it may omit a context or reference a task ID, but doesn't prove that task completed.
4. Disconnect to remove the binding and its stored peer credential. Revoke that credential at the independently operated agent if appropriate. To change a connection's endpoint or credential, disconnect and register again.

If the **peer** refuses access, check the dedicated credential and the permission its operator granted. Signing out of Town won't fix a peer refusal. If the transport instead loses the result, do not automatically send the task again.

**Supported in this version:** a non-tenant A2A 1.0 JSON-RPC interface; explicit anonymous or dedicated bearer authentication; one text message or one completed text artifact. Pending tasks are read with `GetTask`. No streaming, OAuth negotiation, automatic resubmission, or scheduling. If the call times out or the return path breaks, the agent may already have done the work. Check with its operator before sending it again.

This is a bounded interactive call, not an asynchronous job store. At most two outbound operations can run per server process. On timeout or a lost reply, this version does not provide a resumable task handle; correlate the time and task with the peer operator. The ordinary catalog URL is not a forwarding endpoint. The catalog's `is_external_connection` field represents a current binding; descriptive card metadata alone does not authorize an operation.

### Local proof with OpenClaw, no model account

Start the [isolated offline OpenClaw fixture](../scripts/oasis-external-openclaw/README.md#persistent-local-agent-and-relay). On the same machine, run the **native debug** Oasis server with the following configuration and a dedicated local test user:

```sh
unset CP_DOMAIN NANDA_EXTERNAL_A2A_TLS_PROXY_IP
export CP_BIND=127.0.0.1:8080
export NANDA_EXTERNAL_A2A_TARGETS='[{"card_url":"http://127.0.0.1:18916/.well-known/agent-card.json","endpoint":"http://127.0.0.1:18916/a2a/v1"}]'
```

This exact pair is an opt-in exception for the offline fixture in debug builds. It is not a tunnel or a production setting. A hosted or containerized Oasis cannot use the host's loopback address to reach this fixture.

With `OASIS_TEST_USERNAME` and `OASIS_TEST_PASSWORD` set privately for that disposable account, and Docker pointed at the fixture's engine:

```sh
# From the repository root. Change the port only if your local server uses another one.
OASIS_TEST_PORT=8080 node scripts/oasis-external-openclaw/oasis-proof.mjs
```

The check logs in, previews the card, creates a new private connection, and asks for the sum of 8 and 9 **through Oasis** with a fresh random run ID. It requires the answer and run ID to match both in Oasis and in an independent read of the OpenClaw task store, then disconnects. A previous successful task cannot pass. It refuses to replace a pre-existing connection. The model is a local test double; the A2A agent, Oasis server, authenticated request, storage and return path are real. This exercises the local debug destination, not a hosted HTTPS deployment.

## API and scope

All routes are relative to `/api/nanda/external-agents` and require a regular signed-in owner. JSON mutations use `Content-Type: application/json`.

| Action | Request | Result |
| --- | --- | --- |
| Preview | `POST /preview` with `card_url` | Name, full endpoint and whether the card declares required bearer auth |
| Register | `POST` with `card_url`, `expected_endpoint`, `auth_mode` and optional `peer_token` | New private connection and catalog ID (`201`); duplicate registration conflicts |
| List | `GET` | This owner's connections, without credentials |
| Invoke | `POST /{id}/tasks` with `text` | `text`, `result_kind` (`task` or `message`), nullable `context_id` and `task_id` |
| Disconnect | `DELETE /{id}` | `204` |

The generic managed-agent proxy is not changed into an external URL forwarder. A card cannot reroute the outside agent's own model calls or grant MCP access. For the reverse direction, see the [agent-side model and tool gateway instructions](../scripts/oasis-external-openclaw/README.md#if-the-external-agent-wants-to-use-the-oasis-model-or-tool-gateway). Registering an agent does not prove model spend or token savings. This integration does not publish an OpenTelemetry collector.

If the peer also needs to export its own spans, request a tenant-specific, authenticated OTLP ingestion URL from the hosting operator. Do not point it at internal collector ports or reuse the agent's A2A bearer as a telemetry credential. Public intake, tenant attribution, redaction, rate limits and retention are separate hosting work; sending a task does not depend on them.
