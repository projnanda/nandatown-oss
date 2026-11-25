# OpenClaw control agent for Oasis #16

Use a fresh, version-pinned OpenClaw agent to check the external side of onboarding. This is not an agent image deployed or managed by Oasis. The default fixture has **no model credentials and no internet egress** from its agent container. It needs no account and does not use a personal OpenClaw installation.

## Start here: does the agent work on its own?

With the pinned agent image cached, run the [offline control](control/README.md):

```sh
docker compose -f control/compose.yaml run --rm --no-deps control
```

The actual OpenClaw gateway receives one authenticated A2A task, forwards it to a local fixed-answer provider, and returns the stored answer on a separate task read. It must return `17` in the result artifact. HTTP success, a completed status, or echoing the question is not enough. This needs no account and leaves no running container. To exercise the client checks without Docker, run `node --test task-probe.test.mjs control/stub.test.mjs`.

## Persistent local agent and relay

From this directory, using Docker Compose on the local engine (on macOS, `docker context show` should report `orbstack` if that's the engine you intend):

```sh
docker compose up -d --wait
curl -fsS http://127.0.0.1:18916/.well-known/agent-card.json
docker compose exec -T agent node /fixture.mjs smoke
```

The relay exposes only the card and the authenticated A2A JSON-RPC route on a Mac loopback port. The smoke check verifies the card advertises the right agent and exact endpoint, the relay doesn't serve the control UI, anonymous invocation gets 401, and an authenticated GetTask for a new random ID reaches OpenClaw's task store. The expected response is **task not found**, not a successful task. This is a direct control, not an Oasis onboarding result. Hosted Oasis cannot reach this address; do not disable its public-destination policy or publish a tunnel for this test.

OpenClaw generates separate random gateway and peer credentials on first start and persists them in a project-specific Docker volume. Nothing mounts a personal workspace, SSH keys, provider profiles or the Docker socket. Do not print the config, paste a token into an issue, or give Oasis the gateway credential. Only the **peer** bearer belongs in a future private Oasis connection, bound to its destination. Do not copy a personal agent's provider credential.

## What would count as an Oasis onboarding result?

The two checks above are **direct controls**. Neither registers or invokes an agent through Oasis. When a build supporting external onboarding is available, record its commit and the OpenClaw image digest, then:

1. Use the supported onboarding flow for a separately operated agent at an operator-approved address reachable from that Oasis instance. Register its Agent Card without deploying a copy of the agent. Record the supported A2A version and binding; this fixture advertises JSON-RPC 1.0.
2. Configure a destination-specific peer credential using the build's supported secret mechanism. The public card is discovery information, not permission to invoke the agent. If that mechanism or network path is unavailable, record the missing prerequisite and stop.
3. Submit the same addition question **through Oasis**, once. Confirm that OpenClaw received that run and that the result returned to the caller contains `17`. Correlate the task IDs on both sides (record the mapping if different). A direct call or a listing in the UI is not a substitute for this step.
4. Capture the first step that failed, or the successful result and rerun instructions. Include only synthetic inputs, non-secret identifiers and the observed result. If the remote outcome is unknown, look up the existing task rather than sending the question again.

Oasis serving a model or MCP request **from** an agent is the other direction. It does not establish that Oasis can invoke an external agent. Public OpenTelemetry ingestion is a separate deployment decision and is not needed to compare the task results.

## Optional: one real model answer

Only do this when you want outbound provider access and are willing to use your account. The extra Compose file adds ordinary outbound internet access to the test agent; the published port stays on loopback. Use Compose 2.33.1+ for network gateway priority. These are operator steps, **not** part of the offline smoke check:

```sh
docker compose -f compose.yaml -f provider.compose.yaml up -d --wait
docker compose exec agent openclaw config set plugins.allow '["a2a","openai"]' --strict-json
docker compose exec agent openclaw models auth login --agent oasis-test --provider openai --device-code --set-default
docker compose -f compose.yaml -f provider.compose.yaml restart agent
docker compose -f compose.yaml -f provider.compose.yaml up -d --wait
docker compose exec -T agent node /fixture.mjs live
```

Complete the provider's sign-in privately in your browser. The optional check sends **one** synthetic addition task, polls the same task ID, and requires a completed result artifact of exactly `17`. It does not evaluate the model's general capability or prove Oasis routed the call. Do not automatically resend on timeout. The fixture's tool denylist stays in place. If the account cannot use this flow, stop; the fixture does not require switching to paid API-key billing.

To turn off outbound access again, run `docker compose up -d --wait` **without** the override. This does not revoke or erase credentials already saved. To stop the fixture without removing its data, run `docker compose down`.

Sources: [OpenClaw A2A at the fixture version](https://github.com/openclaw/openclaw/blob/v2026.9.3/docs/channels/a2a.md), [OpenClaw provider guide at the fixture version](https://github.com/openclaw/openclaw/blob/v2026.9.3/docs/providers/openai.md).
