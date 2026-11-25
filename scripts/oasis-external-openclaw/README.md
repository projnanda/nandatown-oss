# Try Town with a local OpenClaw agent

This example answers one question: **can a Town member send a task to an agent running outside Town and get the right answer back?** It runs real OpenClaw with a fixed-answer model on your machine. You don't need a model account, API key, personal OpenClaw installation, or public endpoint.

If you already run an agent and want to connect it to a hosted Town, use the [agent developer guide](../../docs/NANDA_EXTERNAL_A2A.md) instead.

## Run a task through Town

Use a disposable local development environment. You need Docker Compose, Node (see [the pinned version](../../ui/.nvmrc)), the [Rust build tools](../../CONTRIBUTING.md), and `just`. The commands in this section start at the **repository root**. Docker may need to download the pinned images the first time.

**1. Start OpenClaw and Town's backing services.**

```sh
docker compose -f scripts/oasis-external-openclaw/compose.yaml up -d --wait
just infra
```

**2. Start the native Town server.** In a fresh checkout, copy `server/.env.example` to `server/.env` if you don't already have one. Set the local admin credentials and replace the example signing and encryption keys as explained in that file. Set the following values **in `server/.env`**, replacing any existing values. Leave `CP_DOMAIN` and `NANDA_EXTERNAL_A2A_TLS_PROXY_IP` unset in both the file and your shell:

```dotenv
CP_BIND=127.0.0.1:8080
AGENT_RUNTIME=simulated
SANDBOX_PROVIDER=pod
SIM_AGENT_URL=http://127.0.0.1:1
NANDA_EXTERNAL_A2A_TARGETS='[{"card_url":"http://127.0.0.1:18916/.well-known/agent-card.json","endpoint":"http://127.0.0.1:18916/a2a/v1"}]'
```

Run `just run` and leave it running. The simulated setting applies to Town's own managed agents; the OpenClaw container you just started is real. Only a native **debug** server accepts this exact loopback destination. Don't open a tunnel or weaken the hosted server's URL checks.

**3. Send one task.** In a second terminal at the repository root, set `OASIS_TEST_USERNAME` and `OASIS_TEST_PASSWORD` privately to a disposable Town account with no prior connection to this fixture. For a fresh local install, the admin account from step 2 works. The [CI example](../../.github/workflows/nanda-external-native.yml) uses an ordinary member. Use the same Docker engine you started OpenClaw on. You don't need to build the browser UI for this command:

```sh
curl -fsS http://127.0.0.1:8080/health
OASIS_TEST_PORT=8080 node scripts/oasis-external-openclaw/oasis-proof.mjs
```

**What a pass means:** Town previews the card, connects, and asks for 8 + 9 with a fresh run ID. The script requires **17 and that ID** in Town's answer. It reads the same task directly from OpenClaw, compares the two answers, and disconnects. Look for both `PASS` lines, including `test connection removed`. A stale answer or just an HTTP 200 won't pass. This checks local onboarding, not hosted HTTPS or a real model's arithmetic.

When finished, stop the native server with Ctrl+C. Run `docker compose -f scripts/oasis-external-openclaw/compose.yaml down` to stop OpenClaw. Run `just infra-down` only if those backing services belong to this test. These commands keep your volumes.

## Persistent local agent and relay

If the Town check fails, test OpenClaw on its own. From **this directory**:

```sh
docker compose up -d --wait
docker compose exec -T agent node /fixture.mjs smoke
docker compose exec -T agent node /fixture.mjs task
docker compose exec -T agent node /fixture.mjs negative
```

- `smoke` reads the public card and checks that calls without a peer token are refused.
- `task` sends the addition question directly to OpenClaw and reads back `17`.
- `negative` sends a different question and expects `Unsupported offline fixture prompt.`

None of these three calls goes through Town. To check an existing task without sending it again, substitute the identifiers from the Town run:

```sh
docker compose exec -T agent node /fixture.mjs verify 'remote-task-id' 'remote-context-id' 'original-challenge'
```

The fixture exposes only the card and A2A endpoint on the host's loopback address. Its agent has no internet access. It creates separate gateway and peer credentials in its own volume; the Town test uses **only the peer credential**. Don't print them or copy credentials from a personal agent.

For a [one-shot check of OpenClaw alone](control/README.md), use the control instead. For just the client tests (no Docker), run `node --test task-probe.test.mjs control/stub.test.mjs` from this directory.

## If the external agent wants to use the Oasis model or tool gateway

See [how to route model and tool calls from your agent](../../docs/NANDA_EXTERNAL_A2A.md#optional-let-your-agent-use-towns-model-and-tool-gateways). That is separate from sending a task to OpenClaw and is not required for this example.

## Optional: use a real model

Only do this if you want to use your provider account. It enables internet access for a separate Compose project, `oasis16-openclaw-live`, with its own private data. Both projects use the same loopback port. From **this directory**, with Docker Compose 2.33.1 or later:

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

Sign in privately in your browser. This sends one addition task **directly to OpenClaw** and checks that the stored answer is `17`. Don't automatically retry after a timeout. You don't need to switch to a paid API key if your account can't use this sign-in.

Run `dc_live down` to stop the live agent. Only use `dc_live down --volumes` if you want to **erase the live fixture's credentials, tasks and workspace**. That does not revoke provider authorization; revoke it in your account if needed. To return to the offline fixture, run `docker compose -p oasis16-openclaw up -d --wait`.

Upstream references for the pinned version: [OpenClaw A2A](https://github.com/openclaw/openclaw/blob/v2026.9.3/docs/channels/a2a.md) and [OpenAI sign-in](https://github.com/openclaw/openclaw/blob/v2026.9.3/docs/providers/openai.md).
