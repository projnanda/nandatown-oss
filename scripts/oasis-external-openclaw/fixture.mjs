// Direct-agent checks for the synthetic local fixture.
import assert from 'node:assert/strict';
import { randomBytes, randomUUID } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { runTask } from './task-probe.mjs';
import { arithmetic, configureStub, startStub, unsupported } from './control/stub.mjs';

const state = '/home/node/.openclaw';
const file = state + '/openclaw.json';
const origin = 'http://127.0.0.1:18916';
const endpoint = origin + '/a2a/v1';
const id = 'oasis-test';
const mode = process.env.OASIS16_MODE;
const stubPort = 18918;
const secret = () => randomBytes(32).toString('hex');

async function init() {
  const config = {
    gateway: {
      mode: 'local', port: 18916, bind: 'lan',
      auth: { mode: 'token', token: secret() }, controlUi: { enabled: false },
    },
    agents: {
      defaults: { skipBootstrap: true, heartbeat: { every: '0m' } },
      list: [{ id, name: 'Oasis test agent', default: true, workspace: state + '/workspace' }],
    },
    plugins: { allow: ['a2a'], entries: { a2a: { enabled: true } }, slots: { memory: 'none' } },
    channels: { a2a: {
      enabled: true, advertisedUrl: origin, exposeAgents: [id],
      peers: { oasis: { token: secret() } },
    } },
    tools: { deny: ['*'], elevated: { enabled: false } },
    browser: { enabled: false }, cron: { enabled: false },
    discovery: { mdns: { mode: 'off' } },
    update: { checkOnStart: false },
  };
  if (mode === 'offline') configureStub(config, stubPort);
  try {
    await writeFile(file, JSON.stringify(config, null, 2), { mode: 0o600, flag: 'wx' });
  } catch (err) {
    if (err.code !== 'EEXIST') throw err;
    // Restarting must never replace an existing config or rotate its credentials.
    const existing = JSON.parse(await readFile(file, 'utf8'));
    assert.ok(existing.agents?.entries?.[id] || existing.agents?.list?.some(a => a.id === id),
      'not the expected fixture volume');
    if (mode === 'offline') requireOffline(existing);
  }
}

async function card(base = origin) {
  const res = await fetch(base + '/.well-known/agent-card.json', {
    redirect: 'error', signal: AbortSignal.timeout(5000),
  });
  assert.equal(res.status, 200, 'card HTTP status');
  const body = await res.json();
  assert.deepEqual(body.skills.map(s => s.id), [id]);
  assert.ok(body.supportedInterfaces.some(i =>
    i.protocolBinding === 'JSONRPC' && i.protocolVersion === '1.0' && i.url === endpoint));
}

async function getAbsentTask(base, token) {
  const requestId = randomUUID();
  const res = await fetch(base + '/a2a/v1', {
    method: 'POST', redirect: 'error', signal: AbortSignal.timeout(5000),
    headers: {
      'Content-Type': 'application/json', 'A2A-Version': '1.0',
      ...(token ? { Authorization: 'Bearer ' + token } : {}),
    },
    body: JSON.stringify({
      jsonrpc: '2.0', method: 'GetTask', id: requestId, params: { id: randomUUID() },
    }),
  });
  return { res, requestId };
}

async function smoke() {
  // Exercise the same gateway that the Mac talks to, not just the agent itself.
  const base = 'http://edge:18916';
  await card(base);
  const admin = await fetch(base + '/', { redirect: 'error', signal: AbortSignal.timeout(5000) });
  assert.equal(admin.status, 404, 'relay exposes only A2A');
  const anon = await getAbsentTask(base);
  assert.equal(anon.res.status, 401, 'RPC without a peer credential');
  const stateConfig = JSON.parse(await readFile(file, 'utf8'));
  const { res, requestId } = await getAbsentTask(base, stateConfig.channels.a2a.peers.oasis.token);
  assert.equal(res.status, 200);
  const body = await res.json();
  assert.equal(body.id, requestId);
  assert.equal(body.error?.code, -32001, 'valid peer reached task lookup');
  console.log('PASS through local relay: card; non-A2A route closed; anonymous RPC rejected; authenticated GetTask reached the task store. No inference or Oasis call.');
}

function requireOffline(config) {
  assert.equal(mode, 'offline', 'use the dedicated offline fixture');
  assert.equal(config.agents.defaults.model?.primary, 'control/addition-stub', 'not the offline fixture config');
  assert.equal(config.models?.providers?.control?.baseUrl, 'http://127.0.0.1:' + stubPort + '/v1');
}

async function task(kind) {
  const base = 'http://edge:18916';
  await card(base);
  const config = JSON.parse(await readFile(file, 'utf8'));
  if (kind === 'live') assert.equal(mode, 'live', 'use the separate opt-in provider project');
  else requireOffline(config);
  const peer = config.channels.a2a.peers.oasis.token;
  async function rpc(method, params, timeoutMs) {
    const req = randomUUID();
    const response = await fetch(base + '/a2a/v1', {
      method: 'POST', redirect: 'error', signal: AbortSignal.timeout(timeoutMs),
      headers: { 'Content-Type': 'application/json', 'A2A-Version': '1.0', Authorization: 'Bearer ' + peer },
      body: JSON.stringify({ jsonrpc: '2.0', id: req, method, params }),
    });
    assert.equal(response.status, 200, 'RPC HTTP status');
    const body = await response.json();
    assert.equal(body.id, req);
    assert.ok(!body.error, 'RPC failed: ' + (body.error?.code ?? 'no code'));
    return body.result;
  }
  if (kind === 'negative') {
    await runTask({ rpc, prompt: 'What is 2 + 2?', expectedAnswer: unsupported });
    console.log('PASS: unsupported prompt returned the explicit fixture refusal, not 17.');
    return;
  }
  const { taskId } = await runTask({ rpc });
  console.log('PASS: directly read OpenClaw task %s; artifact 17 (%s). No Oasis call.', taskId,
    kind === 'live' ? 'opt-in provider' : 'fixed-answer offline provider');
}

const modes = new Map([['init', init], ['card', card], ['smoke', smoke], ['task', () => task('offline')],
  ['negative', () => task('negative')], ['live', () => task('live')], ['stub', () => {
    assert.equal(mode, 'offline');
    return startStub(arithmetic, stubPort);
  }]]);
assert.ok(modes.has(process.argv[2]), 'expected init, card, smoke, task, negative, live or stub');
await modes.get(process.argv[2])();
