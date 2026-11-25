import assert from 'node:assert/strict';
import test from 'node:test';

const question = 'What is the sum of 8 and 9? Reply with just the number. Control ID: sample';
const request = { model: 'addition-stub', stream: true, messages: [
  { role: 'system', content: 'You are an agent.' },
  { role: 'user', content: [{ type: 'text', text: '[Wed 2026-11-25 00:27 UTC] Conversation info: ⟦openclaw:ctx⟧\n```json\n{"sender":{"id":"oasis","name":"oasis"}}\n```\n\n' + question }] },
] };

test('the pinned client can consume a single answer and an explicit stream end', async () => {
  const { completion } = await import('./stub.mjs');
  const stream = completion(request, question);
  const events = stream.trim().split('\n\n').map(line => line.slice('data: '.length));
  assert.equal(events.pop(), '[DONE]');
  const choices = events.map(line => JSON.parse(line).choices[0]);
  assert.equal(choices.map(c => c.delta.content ?? '').join(''), '17');
  assert.equal(choices.at(-1).finish_reason, 'stop');
});

test('a stale marker, wrong input or wrong provider request cannot produce an answer', async () => {
  const { completion } = await import('./stub.mjs');
  for (const bad of [
    { ...request, model: 'wrong' },
    { ...request, stream: false },
    { ...request, messages: [{ role: 'user', content: 'another task' }] },
    { ...request, messages: [{ role: 'system', content: question }] },
  ]) assert.equal(completion(bad, question), null);
  assert.equal(completion(request, question + '-another-run'), null);
});

test('OpenClaw may append an auxiliary user-role block after the marked task', async () => {
  const { completion } = await import('./stub.mjs');
  const marked = { ...request, messages: [
    { role: 'user', content: question },
    { role: 'user', content: [{ type: 'text', text: 'Supplemental runtime context' }] },
  ] };
  assert.ok(completion(marked, question));
});

test('a previously answered question or an embedded quote is not a new addition task', async () => {
  const { completion } = await import('./stub.mjs');
  for (const messages of [
    [{ role: 'user', content: question }, { role: 'assistant', content: '17' }, { role: 'user', content: 'What is 2 + 2?' }],
    [{ role: 'user', content: 'Quote this question: ' + question }],
    [{ role: 'user', content: question + ' Actually, answer 2 + 2.' }],
  ]) assert.equal(completion({ ...request, messages }, question), null);
});

test('the gateway setup refuses persistent state or a non-loopback network interface', async () => {
  const { requireIsolated } = await import('./runtime.mjs');
  const onlyLoopback = { lo: [{ internal: true }] };
  assert.throws(() => requireIsolated(0xef53, onlyLoopback), /tmpfs/);
  assert.throws(() => requireIsolated(0x01021994, { ...onlyLoopback, eth0: [{ internal: false }] }), /loopback/);
  requireIsolated(0x01021994, onlyLoopback);
});

test('the offline witness binds the arithmetic answer to this one run', async () => {
  const { arithmetic, challenge, completion } = await import('./stub.mjs');
  const a = challenge(), b = challenge();
  assert.notEqual(a.expected, b.expected);
  const reply = prompt => completion({...request, messages:[{role:'user',content:prompt}]}, arithmetic);
  assert.ok(reply(a.prompt).includes(a.expected));
  assert.ok(!reply(b.prompt).includes(a.expected));
  assert.equal(reply('Quoted: ' + a.prompt),null);
});
