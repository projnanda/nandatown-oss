// The agent hub's timeline in a real browser (plans/feat-agent-centric.md PR1a; design "Table view", "Responsive"):
// keyboard bucket selection on the axis, a change flag's panel with Escape back to the flag, axe, and 375 px (Table
// first, weekly bars on the chart, no sideways scroll).
import { expect, test } from '@playwright/test'
import { expectAccessible } from './axe'

/** The first seed agent that isn't a coding harness (seed ids are `5eed0000-0001-*`). */
async function openAgent(page: import('@playwright/test').Page) {
  await page.goto('/agents/seed-growth-analyst')
  await expect(page).toHaveURL(/\/agents\/5eed0000-/)
  await expect(page.getByText('peak daily p95')).toBeVisible()
}

test('hub: pick a day by keyboard, open a change flag, Escape returns to it', async ({ page }) => {
  await openAgent(page)
  await expectAccessible(page)

  const axis = page.getByRole('group', {
    name: 'Arrow keys move between buckets; Enter selects one.',
  })
  const stop = axis.locator('button[tabindex="0"]')
  await stop.focus()
  await page.keyboard.press('ArrowLeft')
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/slice=/)
  await expect(page.getByRole('region', { name: /^Selected: / })).toBeVisible()

  const flag = page.getByRole('group', { name: 'Changes' }).getByRole('button').last()
  await flag.click()
  const panel = page.getByRole('dialog')
  await expect(panel.getByRole('link', { name: /^Sessions (on|named)/ })).toBeVisible()
  await expectAccessible(page)
  await page.keyboard.press('Escape')
  await expect(panel).toBeHidden()
  await expect(flag).toBeFocused()
})

test('hub fits a phone @phone', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 })
  await page.goto('/agents/seed-growth-analyst')
  await expect(page).toHaveURL(/\/agents\/5eed0000-/)
  // Phones open on the Table view; the chart draws weeks.
  await expect(page.getByRole('table', { name: 'Activity by bucket' })).toBeVisible()
  await page.getByRole('button', { name: 'Chart', exact: true }).click()
  const axis = page.getByRole('group', {
    name: 'Arrow keys move between buckets; Enter selects one.',
  })
  await expect(axis.getByRole('button').first()).toHaveAccessibleName(/ – /)
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  )
  expect(overflow).toBeLessThanOrEqual(0)
  await expectAccessible(page)
})

// The harness session ledger (PR1b): a recorded harness chat's turns, axe-clean, no sideways page scroll at either width.
for (const width of [1440, 375])
  test(`ledger: a recorded harness session's turns at ${width} px${width < 640 ? ' @phone' : ''}`, async ({
    page,
  }) => {
    await page.setViewportSize({ width, height: 900 })
    await page.goto('/sessions/5eedc000-0000-4000-8000-00000000c004?view=ledger')
    const table = page.getByRole('table', { name: 'Turns in this session' })
    await expect(table).toBeVisible()
    await expect(table.getByRole('row')).toHaveCount(4)
    await expect(page.getByRole('button', { name: 'Export CSV' })).toBeVisible()
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    )
    expect(overflow).toBeLessThanOrEqual(0)
    await expectAccessible(page)
  })
