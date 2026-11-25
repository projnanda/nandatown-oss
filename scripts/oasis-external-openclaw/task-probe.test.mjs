import assert from 'node:assert/strict';
import test from 'node:test';
import { runTask } from './task-probe.mjs';

const id = '19f255ae-c87b-419c-b9fd-c7be4f68a732';
const done = 'TASK_STATE_COMPLETED';
const working = 'TASK_STATE_WORKING';
const output = text => ({ parts: [{ text }] });

function scenario(handler) {
  let time = 0, contextId;
  const calls = [];
  const probe = () => runTask({
    now: () => time,
    sleep: async ms => { time += ms; },
    rpc: async (method, params, timeoutMs) => {
      if (method === 'SendMessage') contextId = params.message.contextId;
      else assert.deepEqual(params, { id });
      calls.push({ method, time, timeoutMs });
      const task = (state, artifacts, overrides) => ({
        id, contextId, status: { state }, artifacts, ...overrides,
      });
      const result = handler({ method, time, timeoutMs, calls, task });
      return method === 'SendMessage' ? { task: result } : result;
    },
  });
  return { probe, calls };
}

test('a 70-second task stays within the pinned gateway rate budget', async () => {
  const { probe, calls } = scenario(({ time, task, calls }) => {
    if (calls.filter(call => call.time > time - 60_000).length > 30) throw Error('rate limited');
    return time < 70_000 ? task(working) : task(done, [output('17')]);
  });
  assert.deepEqual(await probe(), { taskId: id, answer: '17' });
  assert.equal(calls.filter(c => c.method === 'SendMessage').length, 1);
});

test('reads back an immediately completed task from the task store', async () => {
  const { probe, calls } = scenario(({ task }) => task(done, [output('17')]));
  await probe();
  assert.deepEqual(calls.map(c => c.method), ['SendMessage', 'GetTask']);
});

test('one artifact can contain multiple text parts', async () => {
  const { probe } = scenario(({ task }) => task(done, [{ parts: [{ text: '1' }, { text: '7' }] }]));
  await probe();
});

for (const [name, artifacts] of [
  ['two unrelated artifacts', [output('1'), output('7')]],
  ['a provider error marked completed', [output('private-provider-error-body')]],
  ['a non-text result beside the answer', [{ parts: [{ data: {} }, { text: '17' }] }]],
]) {
  test('rejects ' + name + ' without printing the result', async () => {
    const { probe } = scenario(({ task }) => task(done, artifacts));
    await assert.rejects(probe, err =>
      /semantic result/i.test(err.message) && !String(err.stack).includes('private-provider-error-body'));
  });
}

test('never accepts a reply from a different context', async () => {
  const { probe } = scenario(({ method, task }) => method === 'SendMessage'
    ? task(working) : task(done, [output('17')], { contextId: 'other' }));
  await assert.rejects(probe, /context/i);
});

test('a lost submission reply is ambiguous and is never retried', async () => {
  const { probe, calls } = scenario(() => { throw Error('private-network-error'); });
  await assert.rejects(probe, err => /acceptance unknown/i.test(err.message) &&
    !String(err.stack).includes('private-network-error'));
  assert.equal(calls.length, 1);
});

test('an unreadable accepted task reports the known task ID without resubmission', async () => {
  const { probe, calls } = scenario(({ method, task }) => {
    if (method === 'SendMessage') return task(working);
    throw Error('private-network-error');
  });
  await assert.rejects(probe, err => err.message.includes(id) &&
    !String(err.stack).includes('private-network-error'));
  assert.deepEqual(calls.map(c => c.method), ['SendMessage', 'GetTask']);
});

test('pending tasks have one whole-run deadline, including per-request timeouts', async () => {
  const { probe, calls } = scenario(({ task }) => task(working));
  await assert.rejects(probe, err => err.message.includes(id) && /deadline/i.test(err.message));
  assert.ok(calls.every(c => c.timeoutMs > 0 && c.time + c.timeoutMs <= 120_000));
  assert.equal(calls.filter(c => c.method === 'SendMessage').length, 1);
});
