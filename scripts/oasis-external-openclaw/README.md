# OpenClaw control agent for Oasis #16

Use a fresh, version-pinned OpenClaw agent to check the external side of onboarding. This is not an agent image deployed or managed by Oasis. The default fixture has **no model credentials and no internet egress** from its agent container. It needs no account and does not use a personal OpenClaw installation.

To test the whole path, follow the [native Town onboarding guide](../../docs/NANDA_EXTERNAL_A2A.md#local-proof-with-openclaw-no-model-account). Its `oasis-proof.mjs` command checks both the answer returned to the Town caller and the task stored by OpenClaw. The direct controls below help isolate a failure.

## Start here: does the agent work on its own?

With the pinned agent image cached, run the [offline control](control/README.md):

```sh
docker compose -f control/compose.yaml run --rm --no-deps control
```

The actual OpenClaw gateway receives one authenticated A2A task, forwards it to a local fixed-answer provider, and returns the stored answer on a separate task read. It must return `17` in the result artifact. HTTP success, a completed status, or echoing the question is not enough. This needs no account and leaves no running container. To exercise the client checks without Docker, run `node --test task-probe.test.mjs control/stub.test.mjs`.

## Persistent local agent and relay

Use this when you want the same agent to stay up between runs. From this directory, with Docker Compose on the intended local engine:

```sh
docker compose up -d --wait
curl -fsS http://127.0.0.1:18916/.well-known/agent-card.json
docker compose exec -T agent node /fixture.mjs smoke
docker compose exec -T agent node /fixture.mjs task
docker compose exec -T agent node /fixture.mjs negative
```

The relay exposes only the card and the A2A JSON-RPC endpoint on the host's loopback port. `smoke` verifies the card, a closed control-UI route, HTTP 401 without a peer credential, and **task not found** on an authenticated lookup for an unknown task. `task` submits “What is the sum of 8 and 9? Reply with just the number.” It must read back `17`. `negative` submits a different question and must read back `Unsupported offline fixture prompt.` Both use a fixed-answer provider on the agent container's own loopback interface. All three checks talk directly to OpenClaw, not through Oasis.

OpenClaw generates separate random gateway and peer credentials on first start and persists them in a project-specific Docker volume. Nothing mounts a personal workspace, SSH keys, provider profiles or the Docker socket. Do not print the config, paste a token into an issue, or give Oasis the gateway credential. Only the **peer** bearer belongs in the private Oasis connection, bound to its destination. Do not copy a personal agent's provider credential.

## What would count as an Oasis onboarding result?

The one-shot control exits after the test. The persistent agent can answer locally, but **hosted Oasis cannot reach its loopback address**. Use it with a local build only if that build has a supported test route to it; do not disable public-destination protections or publish a tunnel. For a hosted test, use a consenting operator's reachable test agent. Record the build commit and agent version, then:

1. Use the supported onboarding flow to register the separately operated agent's card without deploying a copy. Select an interface by **binding and version**. This fixture advertises JSON-RPC 1.0; another agent may prefer gRPC yet also advertise a compatible JSON-RPC interface.
2. Configure a destination-specific peer credential using the build's supported secret mechanism. The public card is discovery information, not permission to invoke the agent. If that mechanism or network path is unavailable, record the missing prerequisite and stop.
3. For the offline fixture, use the [native onboarding proof](../../docs/NANDA_EXTERNAL_A2A.md#local-proof-with-openclaw-no-model-account). It submits one addition question **through Oasis** with a new UUIDv4 challenge. It requires `17 / <challenge>` and the same answer on a separate read of OpenClaw's task store. To repeat only the read:

   ```sh
   docker compose exec -T agent node /fixture.mjs verify 'remote-task-id' 'remote-context-id' 'original-challenge'
   ```

   Use the actual IDs from that run. This sends only `GetTask`. A prior passing task cannot satisfy a new challenge. Keep the Oasis-side answer as well: the storage read alone does not identify the caller. For a different agent, choose a known semantic outcome and an independent way to verify it.
4. Capture the first step that failed, or the successful result and rerun instructions. Include only synthetic inputs, non-secret identifiers and the observed result. If the remote outcome is unknown, look up the existing task rather than sending the question again.

## If the external agent wants to use the Oasis model or tool gateway

This traffic runs in the opposite direction. On a disposable developer machine, `nasiko agents discover` inventories local agents and reports their cards to the active cluster when logged in. `nasiko connect <discovered-name>` walks the owner through choosing which model calls to route and whether to grant MCP access. Review the target cluster and file before confirming; the generated settings contain credentials. Use only throwaway credentials with the [sample lab](../a2a-discovery-e2e/MANUAL.md#5-connect): its mock model server logs the authorization header.

- **Model calls:** the [sample LangGraph planner](../a2a-discovery-e2e/langgraph/graph.py) needs no code change. Restart it with `OPENAI_BASE_URL=<Oasis origin>/v1` and the **agent-specific Oasis token** as `OPENAI_API_KEY`, supplied through its private environment. Send one message to the planner. As the owner, check its answer and the input/output token counts for that catalog agent in FinOps. The agent does not need the platform's provider key.
- **Tool calls:** load the separate `MCP_GATEWAY_URL` and `MCP_GATEWAY_TOKEN` into an MCP client, as in the [LangGraph snippet](../a2a-discovery-e2e/mcp_langgraph.py). A successful tool list is not a successful tool invocation; check the result of a known, permitted tool.

The gateway enrollment has its own discovered agent ID, separate from the inbound A2A connection ID. Look up model usage under the discovered ID. Revoking one direction does not revoke the other's credentials. Neither configuration lets Oasis invoke the external agent; these checks do not require a public OpenTelemetry collector or establish lower costs.

## Optional: one real model answer

Only do this when you want outbound provider access and are willing to use your account. The extra Compose file starts a **different project and state volume**, `oasis16-openclaw-live`, with internet access for the agent. The published port stays on loopback. Use Compose 2.33.1+ for network gateway priority, from this directory. The optional live project uses the same port, so first stop the default fixture if it is running:

```sh
docker compose -p oasis16-openclaw stop
dc_live() { docker compose -p oasis16-openclaw-live -f compose.yaml -f provider.compose.yaml "$@"; }
dc_live up -d --wait
dc_live exec agent openclaw config set plugins.allow '["a2a","openai"]' --strict-json
dc_live exec agent openclaw models auth login --agent oasis-test --provider openai --device-code --set-default
dc_live restart agent
dc_live up -d --wait
dc_live exec -T agent node /fixture.mjs live
```

Complete the provider's sign-in privately in your browser. The optional check sends **one** synthetic addition task, polls the same task ID, and requires a completed result artifact of exactly `17`. It does not evaluate the model's general capability or prove Oasis routed the call. Do not automatically resend on timeout. The fixture's tool denylist stays in place. If the account cannot use this flow, stop; the fixture does not require switching to paid API-key billing.

When finished, use `dc_live down` to stop the live project and release its port. This preserves its private state for a later run. If you want to **erase that project's test credentials, tasks and workspace**, use `dc_live down --volumes` instead. Removing local data does not revoke the provider's authorization; revoke it separately in your provider account if required. The default fixture is separate and can be restarted with `docker compose -p oasis16-openclaw up -d --wait`.

Sources: [OpenClaw A2A at the fixture version](https://github.com/openclaw/openclaw/blob/v2026.9.3/docs/channels/a2a.md), [OpenClaw provider guide at the fixture version](https://github.com/openclaw/openclaw/blob/v2026.9.3/docs/providers/openai.md).
