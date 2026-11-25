import { mkdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { expect, test, type Page } from '@playwright/test'

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
            json: { text: '17 <demo>', task_id: 'remote-task', context_id: 'remote-context' },
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
