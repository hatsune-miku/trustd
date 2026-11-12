import { expect, test } from '@playwright/test'

test('management logs paginate, switch kinds and render command source literally on mobile', async ({ page }) => {
  const command = `echo '<script>window.logFixtureExecuted = true</script>'\n${'x'.repeat(240)}`
  await page.route('**/v1/admin/logs?**', async (route) => {
    expect(route.request().headers().authorization.startsWith('Bearer ')).toBe(true)
    const query = new URL(route.request().url()).searchParams
    const audit = query.get('kind') === 'audit'
    const older = query.has('before')
    await route.fulfill({
      json: {
        entries: [
          {
            timestamp: '2026-11-12T00:00:00.000Z',
            event: audit ? 'authorization' : 'command.finished',
            ...(audit
              ? { status: 'authorized', source: 'policy' }
              : { command: older ? 'echo older' : command, success: true }),
          },
        ],
        next_before: audit || older ? null : 100,
      },
    })
  })
  await page.goto('/')
  await page.getByLabel('验证方式').selectOption('password')
  await page.getByLabel('Master Password', { exact: true }).fill('browser test')
  await page.getByRole('button', { name: '验证身份', exact: true }).click()
  await page.getByRole('button', { name: '日志', exact: true }).click()
  await expect(page.locator('.logs-command').first()).toHaveText(command)
  expect(await page.evaluate(() => 'logFixtureExecuted' in window)).toBe(false)
  await page.getByRole('button', { name: '更早的日志' }).click()
  await expect(page.getByText('echo older', { exact: true })).toBeVisible()
  await page.screenshot({ path: 'test-results/logs-desktop.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/logs-mobile.png', fullPage: true })
  await page.getByRole('button', { name: '审计日志', exact: true }).click()
  await expect(page.getByText('authorization', { exact: true })).toBeVisible()
  await page.getByText('详情', { exact: true }).click()
  await expect(page.locator('.logs-data')).toContainText('"source": "policy"')
})
