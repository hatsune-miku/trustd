import { expect, test } from '@playwright/test'

test('remote task errors, credential-only sessions, Codex approval and event pagination stay usable', async ({
  page,
}) => {
  const calls: { operation: string; arguments: Record<string, unknown> }[] = []
  let answered = false
  await page.route('**/health', async (route) => {
    const response = await route.fetch()
    await route.fulfill({ json: { ...(await response.json()), daemon: 'user' } })
  })
  await page.route('**/v2/admin/remote', (route) =>
    route.fulfill({
      json: {
        device_id: 'local',
        public_key: 'local-key',
        relay_url: 'https://relay.example',
        connected: true,
        peers: {
          remote: {
            label: 'workstation',
            public_key: 'remote-key',
            allow_execution: true,
            sudo: false,
            secret_keys: ['TEST_KEY'],
          },
        },
      },
    })
  )
  await page.route('**/v2/admin/remote/invoke', async (route) => {
    const body = route.request().postDataJSON()
    calls.push(body)
    let result: unknown = {}
    const agent = {
      id: 'agent-1',
      cwd: '/work',
      status: answered ? 'idle' : 'waiting_user',
      last_message: '',
      questions: answered
        ? []
        : [
            {
              id: 1,
              method: 'item/permissions/requestApproval',
              params: { reason: 'Test network access', permissions: { network: { enabled: true } } },
            },
          ],
    }
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
    if (body.operation === 'agent_start' || body.operation === 'agent_get') result = agent
    if (body.operation === 'agent_respond') {
      answered = true
      result = { accepted: true }
    }
    if (body.operation === 'agent_events')
      result = {
        events: [
          {
            method: 'item/completed',
            params: { item: { type: 'agentMessage', text: body.arguments.offset ? 'second page' : 'first page' } },
          },
        ],
        next_offset: body.arguments.offset ? 20 : 10,
        eof: Boolean(body.arguments.offset),
      }
    await route.fulfill({ json: result })
  })
  await page.goto('/')
  await page.getByLabel('验证方式').selectOption('password')
  await page.getByLabel('Master Password', { exact: true }).fill('browser test')
  await page.getByRole('button', { name: '验证身份', exact: true }).click()
  await page.getByRole('button', { name: '远端主机', exact: true }).click()
  await page.getByRole('button', { name: '连接', exact: true }).click()
  await expect(page.locator('.remote-task')).toContainText('Out-File: output path could not be resolved')
  await page.getByLabel('初始目录').fill('/work')
  await page.locator('#remote-session-keys').fill('TEST_KEY')
  await expect(page.getByLabel('申请管理员 / root 身份')).not.toBeChecked()
  await page.getByRole('button', { name: '进入远端会话', exact: true }).click()
  await page.getByRole('navigation', { name: '远端功能' }).getByRole('button', { name: 'Codex', exact: true }).click()
  await page.getByRole('button', { name: '新建对话', exact: true }).click()
  await expect(page.getByText('Test network access', { exact: true })).toBeVisible()
  expect(calls.some((call) => call.operation === 'agent_respond')).toBe(false)
  await page.getByRole('button', { name: '允许', exact: true }).click()
  await expect(page.getByText('Test network access', { exact: true })).toHaveCount(0)
  expect(calls.find((call) => call.operation === 'session_open')?.arguments).toEqual({
    cwd: '/work',
    sudo: false,
    secret_keys: ['TEST_KEY'],
  })
  expect(calls.find((call) => call.operation === 'agent_respond')?.arguments.result).toEqual({
    permissions: { network: { enabled: true } },
    scope: 'turn',
  })
  await page.getByRole('button', { name: '读取记录', exact: true }).click()
  await expect(page.getByText('first page', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: '继续读取记录', exact: true }).click()
  await expect(page.locator('.remote-output').last()).toContainText('second page')
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
})
