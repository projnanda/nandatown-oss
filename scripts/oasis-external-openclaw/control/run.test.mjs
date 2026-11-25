// In-container integration test; see compose.yaml for its disposable environment.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { runTask } from '../task-probe.mjs';
import test from 'node:test';

test('one SendMessage reaches the local stub; GetTask returns the same completed artifact', async t => {
  const { boot } = await import('./runtime.mjs');
  const question = 'What is the sum of 8 and 9? Reply with just the number. Control ID: ' + randomUUID();
  const { origin, peer, providerCalls, close } = await boot(question);
  t.after(close);

  const card = await fetch(origin + '/.well-known/agent-card.json');
  assert.equal(card.status, 200);
  const { skills, supportedInterfaces } = await card.json();
  assert.deepEqual(skills.map(s => s.id), ['oasis-test']);
  assert.ok(supportedInterfaces.some(i => i.protocolVersion === '1.0' &&
    i.protocolBinding === 'JSONRPC' && i.url === origin + '/a2a/v1'));

  let sends = 0;
  let lastRead;
  async function rpc(method, params, timeoutMs) {
    const id = randomUUID();
    const res = await fetch(origin + '/a2a/v1', {
      method: 'POST', redirect: 'error', signal: AbortSignal.timeout(timeoutMs),
      headers: { 'Content-Type': 'application/json', 'A2A-Version': '1.0', Authorization: 'Bearer ' + peer },
      body: JSON.stringify({ jsonrpc: '2.0', id, method, params }),
    });
    assert.equal(res.status, 200);
    const reply = await res.json();
    assert.equal(reply.id, id);
    assert.equal(reply.error?.code, undefined, 'A2A RPC error');
    if (method === 'SendMessage') sends++;
    if (method === 'GetTask') lastRead = reply.result?.task ?? reply.result;
    return reply.result;
  }

  const { taskId, answer } = await runTask({ rpc, prompt: question });
  assert.equal(sends, 1);
  assert.equal(lastRead?.id, taskId, 'read back the same task');
  assert.equal(lastRead.status?.state, 'TASK_STATE_COMPLETED');
  assert.deepEqual(lastRead.artifacts?.flatMap(a => a.parts?.map(p => p.text)), ['17']);
  assert.equal(answer, '17');
  assert.equal(providerCalls(), 1, 'exactly one request carried this run’s question');
  t.diagnostic('Direct OpenClaw GetTask: TASK_STATE_COMPLETED, artifact="17", one matched stub call. No Oasis or real model.');
});
