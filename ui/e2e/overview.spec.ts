// The Overview at `/` (restored 2026-10-17): the summary bento, Needs you and the Spend card, axe, and no sideways page
// scroll at either width. The Budget burn panel is EE only, so this OSS run never draws it.
import { expect, test } from '@playwright/test'
import { expectAccessible } from './axe'

for (const width of [1440, 375])
  test(`overview at ${width} px${width < 640 ? ' @phone' : ''}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 })
    await page.goto('/?mock=onboarding-done')
    await expect(page.getByTestId('overview-headline')).toBeVisible()
    await expect(page.getByTestId('overview-needs')).toBeVisible()
    await expect(page.getByTestId('overview-spend')).toBeVisible()
    await page.getByRole('radio', { name: 'Last 7 days' }).click()
    await expect(page).toHaveURL(/range=7d/)
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    )
    expect(overflow).toBeLessThanOrEqual(0)
    await expectAccessible(page)
  })
