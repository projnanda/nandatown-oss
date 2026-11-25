import { mkdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { expect, test, type Page } from '@playwright/test'
import { expectAccessible } from './axe'

// Intercept the backend boundary only: the document, controls and requests are the shipped code.
const api = '/api/nanda/external-agents'
const card = 'https://peer.example/.well-known/agent-card.json'
const endpoint = 'https://peer.example/a2a/v1'

// Off in ordinary tests. These PNGs contain only synthetic data; never record the token field filled.
async function capture(page: Page, name: string) {
  const dir = process.env.NANDA_E2E_ARTIFACTS
  if (!dir) return
  await page.evaluate(() => document.fonts.ready.then(() => undefined))
  mkdirSync(dir, { recursive: true })
  await page.screenshot({
    path: join(dir, name + '.png'),
    fullPage: name.startsWith('operator-'),
    animations: 'disabled',
  })
}

test('the OSS catalog links to the server-owned external agent screen', async ({ page }) => {
  await page.goto('/agents?anchor=2026-09-26&demo=1')
  await expect(page.getByRole('link', { name: /Growth Analyst/ }).first()).toBeVisible()
  // CI copies this test (not application code) into a separate checkout of the PR base.
  if (process.env.NANDA_E2E_BASELINE === '1' && process.env.NANDA_E2E_ARTIFACTS) {
    await capture(page, 'catalog-before')
    return
  }
  await expect(page.getByRole('link', { name: 'Connect external A2A agent' })).toHaveAttribute(
    'href',
    '/nanda/external-agents',
  )
  await capture(page, 'catalog-after')
})

test('an operator registers a peer and sends once; an ambiguous result never looks like success', async ({
  page,
}) => {
  const documentHtml = readFileSync(
    new URL('../../server/src/nanda_external/operator.html', import.meta.url),
    'utf8',
  )
  const connection = {
    id: 'one',
    name: 'Example peer (synthetic)',
    card_url: card,
    endpoint,
    auth_mode: 'bearer',
  }
  const registrations: unknown[] = []
  const tasks: unknown[] = []
  let disconnected = false
  await page.route('https://town.example/**', async (route) => {
    const req = route.request()
    const path = new URL(req.url()).pathname
    if (path === '/nanda/external-agents')
      return route.fulfill({ contentType: 'text/html', body: documentHtml })
    if (path === api && req.method() === 'GET') return route.fulfill({ json: [] })
    if (path === api + '/preview')
      return route.fulfill({ json: { name: connection.name, endpoint, requires_bearer: true } })
    if (path === api && req.method() === 'POST') {
      registrations.push(req.postDataJSON())
      return route.fulfill({ status: 201, json: connection })
    }
    if (path === api + '/one/tasks' && req.method() === 'POST') {
      tasks.push(req.postDataJSON())
      return tasks.length === 1
        ? route.fulfill({
            json: {
              result_kind: 'task',
              text: '17 <demo>',
              task_id: 'remote-task',
              context_id: 'remote-context',
            },
          })
        : route.fulfill({ status: 502, json: { error: 'a2a_invalid_result' } })
    }
    if (path === api + '/one' && req.method() === 'DELETE') {
      disconnected = true
      return route.fulfill({ status: 204 })
    }
    return route.abort()
  })
  await page.goto('https://town.example/nanda/external-agents')
  await page.getByLabel('Agent card URL').fill(card)
  await page.getByRole('button', { name: 'Preview card' }).click()
  await expect(page.getByText(endpoint, { exact: true })).toBeVisible()
  await expect(page.getByLabel('Anonymous (no token)')).toBeDisabled()
  const connect = page.getByRole('button', { name: 'Connect agent', exact: true })
  await expect(connect).toBeDisabled()
  await page.getByLabel('Dedicated peer bearer token').check()
  const token = page.getByLabel('Peer token', { exact: true })
  await expect(token).toHaveAttribute('type', 'password')
  await expect(token).toHaveValue('')
  await capture(page, 'operator-preview-synthetic')
  await token.fill('synthetic-peer-token')
  await connect.click()
  await expect(token).toHaveValue('')
  expect(registrations).toEqual([
    {
      card_url: card,
      expected_endpoint: endpoint,
      auth_mode: 'bearer',
      peer_token: 'synthetic-peer-token',
    },
  ])
  const send = page.getByRole('button', { name: 'Send one task' })
  await page.getByLabel('Task to send').fill('8+9')
  // Two clicks in the same event turn must not race two submissions.
  await send.evaluate((node) => {
    ;(node as HTMLButtonElement).click()
    ;(node as HTMLButtonElement).click()
  })
  await expect(page.getByText('17 <demo>', { exact: true })).toBeVisible()
  await expect(page.getByText('8+9', { exact: true })).toBeVisible()
  await expect(page.getByText('remote-task', { exact: true })).toBeVisible()
  await expect(page.getByText('remote-context', { exact: true })).toBeVisible()
  await capture(page, 'operator-result-synthetic')
  await page.getByLabel('Task to send').fill('another task')
  await send.click()
  await expect(page.getByText(/ambiguous or invalid/)).toBeVisible()
  await expect(page.getByText('17 <demo>', { exact: true })).toHaveCount(0)
  expect(tasks).toEqual([{ text: '8+9' }, { text: 'another task' }])
  page.once('dialog', (dialog) => dialog.accept())
  await page.getByRole('button', { name: 'Disconnect', exact: true }).click()
  await expect(page.getByText('Disconnected.', { exact: true })).toBeVisible()
  expect(disconnected).toBe(true)
})

const standalone = () =>
  readFileSync(new URL('../../server/src/nanda_external/operator.html', import.meta.url), 'utf8')
const peer = (id: string) => ({ id, name: id, endpoint, card_url: card, auth_mode: 'none' })

test('normal detail opens the owner’s selected external agent, without a managed chat or deployment', async ({
  page,
  context,
}) => {
  const id = '5eed0000-0001-4000-8000-000000000001'
  await page.goto('/agents/' + id + '?mock=external-connection&tab=builds')
  await expect(page.getByText('Externally hosted', { exact: true })).toBeVisible()
  await expect(page.getByRole('tab')).toHaveCount(0)
  await expect(page.getByRole('link', { name: 'Try it' })).toHaveCount(0)
  await expectAccessible(page)
  const send = page.getByRole('link', { name: 'Send a task' })
  await expect(send).toHaveAttribute('href', '/nanda/external-agents?connection=' + id)
  await context.route('**/nanda/external-agents?connection=*', (route) =>
    route.fulfill({ contentType: 'text/html', body: standalone() }),
  )
  await context.route('**/api/nanda/external-agents', (route) =>
    route.fulfill({ json: [peer('first'), peer(id)] }),
  )
  await send.click()
  await expect(page.getByLabel('Connected agent')).toHaveValue(id)
  await expectAccessible(page)
})

test('preselects the requested owner connection, and distinguishes a direct Message from a completed Task', async ({
  page,
}) => {
  const sent: string[] = []
  await page.route('https://town.example/**', async (route) => {
    const path = new URL(route.request().url()).pathname
    if (path === api) return route.fulfill({ json: [peer('first'), peer('wanted')] })
    if (path.endsWith('/tasks')) {
      sent.push(path)
      return route.fulfill({
        json: {
          result_kind: 'message',
          text: 'Direct answer',
          task_id: 'referenced',
          context_id: null,
        },
      })
    }
    return route.fulfill({ contentType: 'text/html', body: standalone() })
  })
  await page.goto('https://town.example/nanda/external-agents?connection=wanted')
  await expect(page.getByLabel('Connected agent')).toHaveValue('wanted')
  await page.getByLabel('Task to send').fill('question')
  await page.getByRole('button', { name: 'Send one task' }).click()
  await expect(page.getByText('Direct answer', { exact: true })).toBeVisible()
  await expect(page.locator('#result-hint')).toContainText(/direct message/i)
  expect(sent).toEqual([api + '/wanted/tasks'])
  await page.goto('https://town.example/nanda/external-agents?connection=someone-elses')
  await expect(
    page.getByRole('status').filter({ hasText: /not in your connections/i }),
  ).toBeVisible()
  await expect(page.getByLabel('Task to send')).not.toBeVisible()
})

test('a failed logout in another tab erases a pending task and blocks requests across reload until a fresh sign-in', async ({
  page,
  context,
}) => {
  let lists = 0
  let finish!: () => void
  let started!: () => void
  const inFlight = new Promise<void>((r) => {
    started = r
  })
  const waiting = new Promise<void>((r) => {
    finish = r
  })
  const url = '/nanda/external-agents'
  await context.route('**/nanda/external-agents', (route) =>
    route.fulfill({ contentType: 'text/html', body: standalone() }),
  )
  await context.route('**/api/nanda/external-agents**', async (route) => {
    const path = new URL(route.request().url()).pathname
    if (path === api) {
      lists++
      return route.fulfill({ json: [peer('private agent')] })
    }
    if (path.endsWith('/tasks')) {
      started()
      await waiting
      return route.fulfill({
        json: { result_kind: 'task', text: 'old account answer', task_id: 't', context_id: 'c' },
      })
    }
    return route.abort()
  })
  await page.goto(url)
  const origin = await context.newPage()
  await origin.goto('/agents?mock=logout-unavailable')
  await page.getByLabel('Task to send').fill('private draft')
  await page.getByRole('button', { name: 'Send one task' }).click()
  await inFlight
  await origin.getByRole('button', { name: 'Account: admin' }).click()
  await origin.getByRole('menuitem', { name: 'Sign out' }).click()
  await expect(origin.getByText(/this browser may still be signed in/i)).toBeVisible()
  await expect(page.getByRole('link', { name: /Sign in to Town/ })).toBeVisible()
  finish()
  await expect(page.getByLabel('Connected agent')).not.toBeVisible()
  await expect(page.getByRole('button', { name: 'Refresh' })).toBeDisabled()
  await expect(page.locator('#answer')).toHaveText('')
  await page.reload()
  await expect(page.getByRole('button', { name: 'Refresh' })).toBeDisabled()
  expect(lists).toBe(1)
  await origin.getByRole('button', { name: 'Sign in', exact: true }).click()
  await expect(page.getByLabel('Connected agent')).toBeEnabled()
  await origin.evaluate(() => {
    new BroadcastChannel('openruntime.session').postMessage({
      type: 'signed-out',
      failed: true,
      generation: '0',
    })
  })
  await expect(page.getByLabel('Connected agent')).toBeEnabled()
  await expect(page.locator('#answer')).toHaveText('')
})

test('a non-loopback HTTP Town never accepts a peer token', async ({ page }) => {
  await page.route('http://town.example/**', (route) => {
    const path = new URL(route.request().url()).pathname
    if (path === api) return route.fulfill({ json: [] })
    if (path === api + '/preview')
      return route.fulfill({ json: { name: 'Peer', endpoint, requires_bearer: true } })
    return route.fulfill({ contentType: 'text/html', body: standalone() })
  })
  await page.goto('http://town.example/nanda/external-agents')
  await page.getByLabel('Agent card URL').fill(card)
  await page.getByRole('button', { name: 'Preview card' }).click()
  await expect(page.getByLabel('Dedicated peer bearer token')).toBeDisabled()
  await expect(page.getByRole('button', { name: 'Connect agent' })).toBeDisabled()
  await expect(page.getByText(/open Town over HTTPS/i)).toBeVisible()
})

test('a refused transport or peer credential has a specific recovery without logging out', async ({
  page,
}) => {
  const peer = { id: 'one', name: 'Fixture', card_url: card, endpoint, auth_mode: 'bearer' }
  let sends = 0
  await page.route('https://town.example/**', (route) => {
    const path = new URL(route.request().url()).pathname
    if (path === api && route.request().method() === 'GET') return route.fulfill({ json: [peer] })
    if (path === api + '/preview')
      return route.fulfill({ json: { name: peer.name, endpoint, requires_bearer: true } })
    if (path === api)
      return route.fulfill({ status: 400, json: { error: 'secure_transport_required' } })
    if (path === api + '/one/tasks') {
      sends++
      return route.fulfill({ status: 422, json: { error: 'a2a_peer_access_denied' } })
    }
    return route.fulfill({ contentType: 'text/html', body: standalone() })
  })
  await page.goto('https://town.example/nanda/external-agents')
  await page.getByLabel('Agent card URL').fill(card)
  await page.getByRole('button', { name: 'Preview card' }).click()
  await page.getByLabel('Dedicated peer bearer token').check()
  await page.getByLabel('Peer token', { exact: true }).fill('synthetic-only')
  await page.getByRole('button', { name: 'Connect agent', exact: true }).click()
  await expect(page.locator('#add-status')).toContainText('trusted proxy')
  await page.getByLabel('Task to send').fill('hello')
  await page.getByRole('button', { name: 'Send one task' }).click()
  await expect(page.locator('#task-status')).toContainText('peer refused access')
  await expect(page.locator('#signin')).toBeHidden()
  expect(sends).toBe(1)
})
