// Against a disposable, native debug Oasis and the pinned offline OpenClaw
// fixture only. No mocked API, personal credentials, model account, or tracing.
/* global AbortSignal, URL, console, fetch, process */
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { chromium, expect } from '@playwright/test';
import { challenge } from '../../scripts/oasis-external-openclaw/control/stub.mjs';
import { offlinePeerToken, verifyPeerTask } from '../../scripts/oasis-external-openclaw/peer-control.mjs';

const port = process.env.OASIS_TEST_PORT;
assert.ok(/^\d{1,5}$/.test(port) && +port > 0 && +port <= 65535, 'Set the disposable Oasis port');
const origin = 'http://127.0.0.1:' + port;
const card = 'http://127.0.0.1:18916/.well-known/agent-card.json';
const endpoint = 'http://127.0.0.1:18916/a2a/v1';
const connections = '/api/nanda/external-agents';
const uuid = /^[\da-f]{8}(?:-[\da-f]{4}){3}-[\da-f]{12}$/;
const adminUser = process.env.OASIS_TEST_ADMIN, adminPassword = process.env.OASIS_TEST_ADMIN_PASSWORD;
assert.ok(adminUser && adminPassword, 'Set credentials for the disposable Oasis bootstrap user');
async function request(method, path, data, token) {
  return fetch(origin + path, { method, redirect: 'error', signal: AbortSignal.timeout(10_000),
    headers: {...(data ? {'content-type':'application/json'} : {}), ...(token ? {authorization:'Bearer ' + token} : {})},
    body:JSON.stringify(data) });
}
async function fillSecret(locator, value, label) {
  // Playwright includes fill("...") in actionability failure logs.
  // Only the fixed label may reach the caller, including outside Actions.
  try { await locator.fill(value); } catch { throw new Error('Could not fill ' + label); }
}
const login = await request('POST', '/api/auth/login', {username:adminUser, password:adminPassword});
assert.equal(login.status, 200, 'Bootstrap login');
const admin = (await login.json()).token;
const username = 'a2a-member-' + randomUUID();
const create = await request('POST', '/api/users', {username, email:username+'@example.invalid', role:'member', is_superuser:false}, admin);
assert.equal(create.status, 201, 'Create temporary member');
const member = await create.json();
assert.match(member.id, uuid);
let browser, memberSession, cleanupFailed = false;
try {
  browser = await chromium.launch();
  const page = await browser.newPage({serviceWorkers:'block'});
  // A live UI may have optional analytics. Keep this fixture entirely local.
  await page.route('**/*', route => new URL(route.request().url()).origin === origin ? route.continue() : route.abort());
  await page.goto(origin+'/login');
  await page.locator('#login-username').fill(username);
  await fillSecret(page.locator('#login-password'), member.access_secret, 'the disposable member password');
  const [signedIn] = await Promise.all([
    page.waitForResponse(r => r.url() === origin+'/api/auth/login' && r.request().method() === 'POST'),
    page.waitForURL(url => url.pathname !== '/login'), page.locator('button[type=submit]').click(),
  ]);
  assert.equal(signedIn.status(), 200);
  const me = await signedIn.json();
  assert.equal(me.user_id, member.id, 'Browser must be the temporary member');
  assert.equal(me.is_superuser, false);
  assert.ok(typeof me.token === 'string' && me.token, 'Missing member session');
  memberSession = me.token;
  await page.goto(origin+'/agents');
  await page.getByRole('link', {name:'Connect external A2A agent'}).click();
  await page.getByLabel('Agent card URL').fill(card);
  await page.getByRole('button', {name:'Preview card'}).click();
  await expect(page.locator('#preview-name')).toHaveText('Oasis test agent');
  await page.locator('#bearer').check();
  await fillSecret(page.getByLabel('Peer token', {exact:true}), offlinePeerToken(), 'the offline peer bearer');
  await page.getByRole('button', {name:'Connect agent', exact:true}).click();
  await expect(page.locator('#add-status')).toContainText('connected');
  const id = await page.getByLabel('Connected agent').inputValue();
  assert.match(id, uuid);
  await page.goto(origin+'/agents');
  await page.locator('a[href="/agents/'+id+'"]').first().click();
  await page.getByRole('link', {name:'Send a task', exact:true}).click();
  await expect(page.getByLabel('Connected agent')).toHaveValue(id);
  const fresh = challenge();
  await page.getByLabel('Task to send').fill(fresh.prompt);
  const [reply] = await Promise.all([
    page.waitForResponse(r => r.url() === origin + connections + '/' + id + '/tasks' && r.request().method() === 'POST', {timeout:135_000}),
    page.getByRole('button', {name:'Send one task'}).click(),
  ]);
  assert.equal(reply.status(), 200, 'Native member task');
  assert.equal((await reply.json()).result_kind, 'task', 'A referenced Message is not a completed Task');
  await expect(page.locator('#answer')).toHaveText(fresh.expected);
  verifyPeerTask(await page.locator('#task-id').innerText(), await page.locator('#context-id').innerText(), fresh.nonce);
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', {name:'Disconnect',exact:true}).click();
  await expect(page.locator('#task-status')).toHaveText('Disconnected.');
  const remaining = await page.evaluate(async () => (await fetch('/api/nanda/external-agents')).json());
  assert.deepEqual(remaining, []);
} catch (error) {
  // The browser may have lost the registration response, so do not depend on
  // a captured ID. Never use the admin session to discover or delete bindings.
  if (memberSession) try {
    const listed = await request('GET', connections, undefined, memberSession);
    assert.equal(listed.status, 200);
    const matches = (await listed.json()).filter(c => c.card_url === card && c.endpoint === endpoint);
    assert.ok(matches.length <= 1, 'Ambiguous test connection');
    if (matches.length) {
      assert.match(matches[0].id, uuid);
      const removed = await request('DELETE', connections + '/' + matches[0].id, undefined, memberSession);
      assert.ok(removed.status === 204 || removed.status === 404);
    }
  } catch { console.error('Cleanup could not confirm removal of the disposable member’s fixture binding.'); }
  throw error;
} finally {
  const cleanup = await Promise.allSettled([browser?.close(), (async () => {
    const retired = await request('POST', '/api/users/'+member.id+'/deactivate', {}, admin);
    assert.ok(retired.ok);
  })()]);
  if (cleanup.some(r => r.status === 'rejected')) {
    cleanupFailed = true;
    console.error('Could not confirm browser close and disposable member deactivation.');
  }
}
assert.ok(!cleanupFailed, 'Disposable smoke cleanup failed');
console.log('PASS: normal member browser → catalog → native connection → new task; matching nonce verified in OpenClaw; disconnected.');
