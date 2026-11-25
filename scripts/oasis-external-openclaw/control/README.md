# Offline OpenClaw task control

Does the pinned OpenClaw A2A gateway accept an authenticated task, pass its
input to a provider, and store a retrievable answer? This test runs the actual
gateway with a **fixed-answer test double**, not a language model. It calls
OpenClaw directly; Oasis, its onboarding flow, and the parent relay are not in
the path.

From this directory, with Docker Compose and the parent fixture's pinned
OpenClaw image already cached in the local engine:

```sh
docker compose -f compose.yaml run --rm --no-deps control
```

On macOS, use `docker --context orbstack compose -f compose.yaml run --rm --no-deps control`
to select OrbStack explicitly. The command never pulls. If the image is missing,
download it once to the same engine before running offline:

```sh
docker pull ghcr.io/openclaw/openclaw:2026.9.3@sha256:6cb72e1599b3b76e2ea3dcc2f4dd7f112247367fbecadfcdee8caa7423f4989b
```

Use `docker --context orbstack pull …` if you selected OrbStack explicitly.
To run only the unit checks (Node.js 22+, no Docker): `node --test stub.test.mjs`.

The one-shot container creates fresh state, checks the Agent Card, then uses
the [same task probe as the optional real-provider run](../task-probe.mjs).
It sends one addition question with a fresh marker. The local stub streams
`17` only if it receives that marked question as a user message for the
expected model. The test requires exactly one matched provider call and
an actual `GetTask` read of the same completed task with one text part
equal to `17`. A terminal state by itself is not a pass: a provider
error can also appear as text in a completed task.

The Compose project is separate from `oasis16-openclaw`. It publishes no ports,
has no network interface beyond loopback, and mounts only fixture sources
read-only. The gateway, peer tokens, workspace, tasks, and cache are temporary.
The provider configuration uses an inert placeholder, not a provider credential.
There is no API sign-in, provider account, paid inference, external service,
or manual cleanup. No gateway or peer token is printed. The fixture does not
test arithmetic ability, real-provider access, Oasis routing, or hosted reachability.
