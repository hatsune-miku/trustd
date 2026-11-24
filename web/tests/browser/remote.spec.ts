import { expect, test } from '@playwright/test'

test('remote task errors and credential-only sessions stay usable', async ({ page }) => {
  const calls: { operation: string; arguments: Record<string, unknown> }[] = []
  await page.route('**/health', async (route) => {
    const response = await route.fetch()
    await route.fulfill({ json: { ...(await response.json()), daemon: 'user' } })
  })
  await page.route('**/v2/admin/remote', (route) =>
    route.fulfill({
      json: {
        device_id: 'local',
        public_key: 'local-key',
        joined: true,
        backend: 'https://backend.example',
        connected: true,
        peers: { remote: { label: 'workstation', public_key: 'remote-key' } },
      },
    })
  )
  await page.route('**/v2/admin/remote/invoke', async (route) => {
    const body = route.request().postDataJSON()
    calls.push(body)
    let result: unknown = {}
    if (body.operation === 'host_info') result = { platform: 'linux', arch: 'x86_64', version: 'test', identities: {} }
    if (body.operation === 'session_open')
      result = { session_ref: 'remote-session', privilege_levels: ['current-user'] }
    if (body.operation === 'job_list')
      result = {
        jobs: [
          {
            id: 'failed-job',
            command: 'calc',
            status: 'failed',
            result: { exit_code: 1, stderr: 'Out-File: output path could not be resolved' },
          },
        ],
        next_offset: null,
      }
    await route.fulfill({ json: result })
  })
  // A user daemon has no password; sign in through a `trustd login-url` link.
  await page.route('**/v1/admin/login', (route) =>
    route.fulfill({ json: { session: 'fixture-session', expires_at: 4102444800 } })
  )
  await page.goto('/#ticket=fixture-ticket')
  await page.getByRole('button', { name: '远端主机', exact: true }).click()
  await page.getByRole('button', { name: '连接', exact: true }).click()
  await expect(page.locator('.remote-task')).toContainText('Out-File: output path could not be resolved')
  await page.getByLabel('初始目录').fill('/work')
  await page.locator('#remote-session-keys').fill('TEST_KEY')
  await expect(page.getByLabel('申请管理员 / root 身份')).not.toBeChecked()
  await page.getByRole('button', { name: '进入远端会话', exact: true }).click()
  expect(calls.find((call) => call.operation === 'session_open')?.arguments).toEqual({
    cwd: '/work',
    sudo: false,
    secret_keys: ['TEST_KEY'],
  })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
})
