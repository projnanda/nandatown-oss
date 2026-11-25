import assert from 'node:assert/strict';
import test from 'node:test';
import { runTask, verifyStoredTask } from './task-probe.mjs';

const id = '19f255ae-c87b-419c-b9fd-c7be4f68a732';
const done = 'TASK_STATE_COMPLETED';
const working = 'TASK_STATE_WORKING';
const output = text => ({ parts: [{ text }] });

function scenario(handler) {
  let time = 0, contextId;
  const calls = [];
  const probe = (options = {}) => runTask({
    ...options,
    now: () => time,
    sleep: async ms => { time += ms; },
    rpc: async (method, params, timeoutMs) => {
      if (method === 'SendMessage') contextId = params.message.contextId;
      else assert.deepEqual(params, { id });
      calls.push({ method, time, timeoutMs, contextId });
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
  const result = await probe();
  assert.deepEqual(result, { taskId: id, contextId: calls[0].contextId, answer: '17' });
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

test('requires the exact expected refusal for the unsupported offline question', async () => {
  const expectedAnswer = 'Unsupported offline fixture prompt.';
  const { probe, calls } = scenario(({ method, task }) => method === 'SendMessage'
    ? task(working) : task(done, [output(expectedAnswer)]));
  const result = await probe({ expectedAnswer });
  assert.deepEqual(result, { taskId: id, contextId: calls[0].contextId, answer: expectedAnswer });
  assert.deepEqual(calls.map(c => c.method), ['SendMessage', 'GetTask']);
  const wrong = scenario(({ task }) => task(done, [output('17')]));
  await assert.rejects(() => wrong.probe({ expectedAnswer }), /semantic result/i);
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

test('an operator can verify an existing task in one read, without another submission', async () => {
  const contextId = 'ses_local-fixture-1';
  const reads = [];
  await verifyStoredTask({ taskId: id, contextId, getTask: async requested => {
    reads.push(requested);
    return { id, contextId, status: { state: done }, artifacts: [output('17')] };
  } });
  assert.deepEqual(reads, [id]);
});

test('an unavailable or unrelated stored result never counts as this run', async () => {
  const contextId = 'unique-to-this-run';
  const stored = { id, contextId, status: { state: done }, artifacts: [output('17')] };
  for (const [change, match] of [
    [{ contextId: 'previous-run' }, /context/i], [{ id: 'wrong-task' }, /task/i],
    [{ status: { state: working } }, /pending/i], [{ artifacts: [output('16')] }, /semantic/i],
  ]) {
    await assert.rejects(() => verifyStoredTask({
      taskId: id, contextId, getTask: async () => ({ ...stored, ...change }),
    }), match);
  }
  await assert.rejects(() => verifyStoredTask({
    taskId: id, contextId, getTask: async () => { throw Error('private-response-body'); },
  }), err => /could not read/i.test(err.message) && !err.stack.includes('private-response-body'));
});

test('a missing correlation ID is not treated as a match or sent to the store', async () => {
  let reads = 0;
  for (const [taskId, contextId] of [['wrong-format', 'ctx1'], [id, ''], [id, undefined], [id, '  ']]) {
    await assert.rejects(() => verifyStoredTask({
      taskId, contextId, getTask: async () => { reads++; },
    }), /identifiers/i);
  }
  assert.equal(reads, 0);
});

test('correct stored IDs and an earlier answer cannot witness a fresh challenge', async () => {
  const contextId = 'same-context', oldAnswer = '17 / previous-run', fresh = '17 / current-run';
  let reads = 0;
  const read = () => { reads++; return {id, contextId,status:{state:done},artifacts:[output(oldAnswer)]}; };
  await verifyStoredTask({taskId:id,contextId,getTask:read,expectedAnswer:oldAnswer});
  await assert.rejects(() => verifyStoredTask({taskId:id,contextId,getTask:read,expectedAnswer:fresh}), /semantic/);
  assert.equal(reads,2);
});

test('the caller accepts a bounded opaque peer-assigned context, not just a bare task UUID', async () => {
  const { validContext } = await import('./task-probe.mjs');
  assert.equal(validContext('ctx-' + id), true);
  assert.equal(validContext('   '), false);
  assert.equal(validContext('x'.repeat(10000)), false);
});
