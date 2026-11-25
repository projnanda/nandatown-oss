// Exercise the native Oasis server and the known offline fixture. Never send a
// credential to a hostname from a card, CLI argument, or redirect.
import assert from 'node:assert/strict';
import { challenge } from './control/stub.mjs';
import { offlinePeerToken, verifyPeerTask } from './peer-control.mjs';
import { validContext } from './task-probe.mjs';

const port = process.env.OASIS_TEST_PORT ?? '8080';
assert.ok(/^\d{1,5}$/.test(port) && +port > 0 && +port <= 65535, 'Invalid local Oasis port');
const origin = 'http://127.0.0.1:' + port;
const card_url = 'http://127.0.0.1:18916/.well-known/agent-card.json';
const endpoint = 'http://127.0.0.1:18916/a2a/v1';
const api = '/api/nanda/external-agents';
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const user = process.env.OASIS_TEST_USERNAME, password = process.env.OASIS_TEST_PASSWORD;
assert.ok(user && password, 'Set the username and password of a disposable local Oasis account.');
let token;
async function request(path, method = 'GET', data) {
  return fetch(origin + path, {method, redirect:'error', signal:AbortSignal.timeout(140_000),
    headers:{...(token ? {authorization:'Bearer ' + token} : {}), ...(data ? {'content-type':'application/json'} : {})},
    body:data ? JSON.stringify(data) : undefined});
}
async function expect(response, statuses, label) {
  assert.ok(statuses.includes(response.status), label + ': HTTP ' + response.status);
  return response;
}
const login = await expect(await request('/api/auth/login','POST',{username:user,password}),[200],'Oasis login');
token = (await login.json()).token;
assert.ok(typeof token === 'string' && token.length > 0,'Missing local Oasis session');
const preview = await (await expect(await request(api+'/preview','POST',{card_url}),[200],'Authenticated card preview')).json();
assert.equal(preview.endpoint,endpoint);
assert.equal(preview.name,'Oasis test agent');
const before = await (await expect(await request(api),[200],'List before registration')).json();
assert.ok(Array.isArray(before) && before.every(x=>x.card_url !== card_url),'Existing connection: use a fresh test owner; no changes made.');
const peer = offlinePeerToken();
const created = await expect(await request(api,'POST',{card_url,expected_endpoint:endpoint,auth_mode:'bearer',peer_token:peer}),[201],'New private connection');
const conn = await created.json();
assert.match(conn.id,uuid);
const path = api+'/'+conn.id;
try {
  const list = await (await expect(await request(api),[200],'Owner catalog')).json();
  assert.ok(list.some(x=>x.id === conn.id && x.endpoint === endpoint && x.auth_mode === 'bearer'));
  const {data:detail} = await (await expect(await request('/api/agents/'+conn.id),[200],'Standard catalog detail')).json();
  assert.equal(detail?.id,conn.id,'Standard catalog returned a different agent');
  assert.equal(detail.protocolVersion,'1.0');
  assert.deepEqual(detail.defaultInputModes,['text/plain']);
  assert.deepEqual(detail.defaultOutputModes,['text/plain']);
  assert.equal(detail.metadata?.nanda_external?.managed_proxy,false);
  assert.equal(detail.metadata.nanda_external.task_path,path+'/tasks');
  const fresh = challenge();
  const run = await (await expect(await request(path+'/tasks','POST',{text:fresh.prompt}),
    [200],'Oasis task: do not resubmit on failure')).json();
  assert.equal(run.result_kind,'task','OpenClaw should return a Task, not a direct Message');
  assert.ok(run.text === fresh.expected,'Oasis caller did not receive the run-specific answer');
  assert.match(run.task_id,uuid);
  assert.ok(validContext(run.context_id),'Expected a peer-assigned context ID');
  verifyPeerTask(run.task_id,run.context_id,fresh.nonce);
  console.log('PASS: Oasis and the OpenClaw task store matched challenge %s, task %s, context %s.',fresh.nonce,run.task_id,run.context_id);
} finally {
  await expect(await request(path,'DELETE'),[204],'Disconnect only this test connection');
}
const after = await (await expect(await request(api),[200],'List after disconnect')).json();
assert.ok(after.every(x=>x.id!==conn.id),'Disconnected connection still visible.');
console.log('PASS: test connection removed. No paid model, deployment or public endpoint.');
