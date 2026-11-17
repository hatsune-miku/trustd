import { readFile } from 'node:fs/promises'

import { type Page, expect, test } from '@playwright/test'

async function login(page: Page) {
  await page.goto('/')
  await expect(page.getByLabel('验证方式')).toHaveValue('password')
  await page.getByLabel('Master Password', { exact: true }).fill('browser test')
  await page.getByRole('button', { name: '验证身份', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Agent 授权' })).toBeVisible()
}

test('an unpaired system daemon signs in with its password or TOTP, never a localhost passkey', async ({ page }) => {
  await page.goto('/')
  const method = page.getByLabel('验证方式')
  await expect(method).toHaveValue('password')
  await expect(method.locator('option[value="passkey"]')).toHaveCount(0)
  await expect(page.getByRole('button', { name: '使用通行密钥登录' })).toHaveCount(0)
  await page.screenshot({ path: 'test-results/desktop-login.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/mobile-login.png', fullPage: true })
  await method.selectOption('totp')
  await expect(page.getByLabel('6 位动态码')).toHaveAttribute('inputmode', 'numeric')
  await method.selectOption('password')
  await expect(page.getByLabel('Master Password', { exact: true })).toBeVisible()
})

test('the user daemon signs in with the owner passkey on the backend or a local ticket', async ({ page }) => {
  const logins: unknown[] = []
  await page.route('**/health', async (route) => {
    const response = await route.fetch()
    await route.fulfill({ json: { ...(await response.json()), daemon: 'user' } })
  })
  await page.route('**/v1/admin/login/passkey/start', (route) =>
    route.fulfill({
      json: {
        login_id: 'fixture-login',
        verification_uri: 'https://backend.example/#sign=ABC123',
        user_code: 'ABC123',
        expires_in: 300,
        interval: 2,
      },
    })
  )
  let confirmed = false
  await page.route('**/v1/admin/login/passkey/poll', (route) =>
    route.fulfill({ json: confirmed ? { session: 'fixture-session', expires_at: 4102444800 } : { pending: true } })
  )
  await page.route('**/v1/admin/login', async (route) => {
    logins.push(route.request().postDataJSON())
    await route.fulfill({ json: { session: 'fixture-session', expires_at: 4102444800 } })
  })
  // The approval page opens in a new tab; record it instead of opening one.
  await page.addInitScript(() => {
    const opened: string[] = []
    Object.assign(window, { openedTabs: opened })
    window.open = () =>
      ({
        location: {
          set href(value: string) {
            opened.push(value)
          },
        },
        close() {},
      }) as unknown as Window
  })
  await page.goto('/')
  await expect(page.getByText('trustd login-url')).toBeVisible({ timeout: 15000 })
  await expect(page.getByLabel('验证方式')).toHaveCount(0)
  await page.getByRole('button', { name: '使用通行密钥登录' }).click()
  await expect(page.locator('.login-code')).toHaveText('ABC123', { timeout: 15000 })
  expect(await page.evaluate(() => (window as unknown as { openedTabs: string[] }).openedTabs)).toEqual([
    'https://backend.example/#sign=ABC123',
  ])
  confirmed = true
  await expect(page.getByRole('heading', { name: 'Agent 授权' })).toBeVisible({ timeout: 10000 })
  expect(logins).toEqual([])
  // The user daemon has no password, TOTP or localhost passkey to manage.
  await expect(page.getByRole('button', { name: '验证方式', exact: true })).toHaveCount(0)
  await page.getByRole('button', { name: '退出管理' }).click()
  // A link from `trustd login-url` signs in once and clears the ticket.
  await page.goto('/#ticket=fixture-ticket')
  await expect(page.getByRole('heading', { name: 'Agent 授权' })).toBeVisible({ timeout: 15000 })
  expect(logins).toEqual([{ credential: { type: 'ticket', value: 'fixture-ticket' } }])
  expect(new URL(page.url()).hash).toBe('')
})

test('path-scoped policies show full paths and replace token issuance', async ({ page }) => {
  const agents = [
    {
      id: 'first',
      name: 'codex.exe',
      path: 'C:\\Users\\Owner\\Applications\\Codex with spaces\\codex.exe',
      processes: [
        { pid: 123, started_at: 1 },
        { pid: 456, started_at: 1 },
      ],
      policy: 'ask',
      revision: 0,
    },
    {
      id: 'second',
      name: 'codex.exe',
      path: 'D:\\Other install\\' + 'Long directory '.repeat(15) + '\\codex.exe',
      processes: [],
      policy: 'deny',
      revision: 0,
    },
    {
      id: 'unresolved',
      name: 'claude.exe',
      path: null,
      processes: [{ pid: 789, started_at: 1 }],
      policy: 'ask',
      revision: 0,
    },
  ]
  await page.route('**/v1/admin/agents', (route) => route.fulfill({ json: { agents } }))
  await page.route('**/v1/admin/agents/policy', async (route) => {
    const body = route.request().postDataJSON()
    const agent = agents.find((item) => item.id === body.id)!
    expect(body.revision).toBe(agent.revision)
    expect(body.command_id).toMatch(/^[\da-f-]{36}$/)
    expect(body.path).toBeUndefined()
    agent.policy = body.policy
    agent.revision += 1
    await route.fulfill({ json: { saved: true } })
  })
  await login(page)
  await expect(page.getByText(agents[0].path!, { exact: true })).toBeVisible()
  await expect(page.getByText(agents[1].path!, { exact: true })).toBeVisible()
  await expect(page.getByText('运行中 · PID 123、456')).toBeVisible()
  const first = page.getByRole('combobox', { name: `codex.exe ${agents[0].path} 的授权状态`, exact: true })
  const second = page.getByRole('combobox', { name: `codex.exe ${agents[1].path} 的授权状态`, exact: true })
  for (const policy of ['once', 'trust', 'ask', 'deny']) {
    await first.selectOption(policy)
    await expect(first).toHaveValue(policy)
    await expect(first).toBeEnabled()
    await expect(second).toHaveValue('deny')
  }
  await expect(page.getByRole('combobox', { name: 'claude.exe unresolved 的授权状态' })).toBeDisabled()
  await expect(page.getByRole('button', { name: '确认授权并签发' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: '复制授权文本' })).toHaveCount(0)
  await page.screenshot({ path: 'test-results/desktop-agents.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await expect(page.getByText(agents[1].path!, { exact: true })).toBeVisible()
  await page.screenshot({ path: 'test-results/mobile-agents.png', fullPage: true })
})

test('mobile layout does not overflow and settings remain available', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await login(page)
  await page.getByRole('button', { name: '验证方式', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Authenticator', exact: true })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/mobile-credentials.png', fullPage: true })
  await page.getByRole('button', { name: '桌面授权', exact: true }).click()
  await expect(page.getByRole('button', { name: '清除所有记忆' })).toHaveCount(0)
  await page.getByRole('button', { name: '已授权会话', exact: true }).click()
  await expect(page.getByRole('heading', { name: '已授权会话' })).toBeVisible()
})

test('consent content is literal text and returns choice plus note only through native IPC', async ({ page }) => {
  await page.setViewportSize({ width: 640, height: 780 })
  await page.addInitScript(() => {
    window.__LOCALSHELLD_PROMPT__ = {
      id: 'test',
      agent_name: 'Test agent',
      agent_path: '/Applications/Agent with spaces/Test agent',
      request_reason: 'literal <script>alert(1)</script>\nsecond line',
      scope: '仅回归测试，不执行命令',
      secret_keys: ['JENKINS_CI_TOKEN', 'OPENAI_API_KEY'],
      expires_at_ms: Date.now() + 60000,
    }
    window.ipc = {
      postMessage: (message: string) => {
        document.body.dataset.reply = message
      },
    }
  })
  await page.goto('/consent.html')
  await expect(page.getByLabel('代理提供的请求缘由')).toHaveText('literal <script>alert(1)</script>\nsecond line')
  await expect(page.getByLabel('请求注入的凭据')).toHaveValue('JENKINS_CI_TOKEN\nOPENAI_API_KEY')
  await expect(page.getByLabel('请求注入的凭据')).toHaveAttribute('readonly', '')
  await expect(page.getByRole('button', { name: '接受', exact: true })).toBeInViewport()
  await page.screenshot({ path: 'test-results/consent-secrets.png', fullPage: true })
  await page.getByLabel('附加说明', { exact: true }).fill('additional note')
  await page.getByRole('checkbox').check()
  await page.getByRole('button', { name: '接受', exact: true }).click()
  expect(await page.evaluate(() => JSON.parse(document.body.dataset.reply!))).toEqual({
    choice: 'accept',
    remember: true,
    btw: 'additional note',
  })
})

test('credentials require uppercase keys and support write-only creation, replacement and deletion', async ({
  page,
}) => {
  await login(page)
  await page.getByRole('button', { name: '凭据', exact: true }).click()
  const keyInput = page.getByLabel('Key', { exact: true })
  const valueInput = page.getByLabel('值', { exact: true })
  await expect(valueInput).toHaveAttribute('type', 'text')
  await keyInput.fill('lower_case')
  await valueInput.fill('fixture-browser-value')
  await page.getByRole('button', { name: '添加凭据', exact: true }).click()
  expect(await keyInput.evaluate((input: HTMLInputElement) => input.validity.patternMismatch)).toBe(true)
  await keyInput.fill('BROWSER_TEST_TOKEN')
  const savedResponse = page.waitForResponse((response) => response.url().endsWith('/v1/admin/secrets/put'))
  await page.getByRole('button', { name: '添加凭据', exact: true }).click()
  expect(await (await savedResponse).json()).toEqual({ saved: true })
  await expect(page.getByText('凭据已保存。', { exact: true })).toBeVisible()
  await expect(valueInput).toHaveValue('')
  const row = page.getByRole('listitem').filter({ hasText: 'BROWSER_TEST_TOKEN' })
  await expect(row).toBeVisible()
  await expect(page.getByText('fixture-browser-value', { exact: true })).toHaveCount(0)
  await page.screenshot({ path: 'test-results/desktop-secrets.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/mobile-secrets.png', fullPage: true })
  await row.getByRole('button', { name: '更换', exact: true }).click()
  await expect(keyInput).toHaveAttribute('readonly', '')
  await expect(page.getByLabel('新值', { exact: true })).toHaveValue('')
  await page.getByLabel('新值', { exact: true }).fill('fixture-browser-rotated')
  await page.getByRole('button', { name: '保存新值', exact: true }).click()
  await expect(page.getByLabel('值', { exact: true })).toHaveValue('')
  await page.reload()
  await login(page)
  await page.getByRole('button', { name: '凭据', exact: true }).click()
  await expect(page.getByRole('listitem').filter({ hasText: 'BROWSER_TEST_TOKEN' })).toBeVisible()
  page.once('dialog', (dialog) => dialog.accept())
  await page
    .getByRole('listitem')
    .filter({ hasText: 'BROWSER_TEST_TOKEN' })
    .getByRole('button', { name: '删除', exact: true })
    .click()
  await expect(page.getByText('凭据已删除。', { exact: true })).toBeVisible()
  await expect(page.getByRole('listitem').filter({ hasText: 'BROWSER_TEST_TOKEN' })).toHaveCount(0)
})

test('desktop overview renders cleanly', async ({ page }) => {
  await login(page)
  await page.screenshot({ path: 'test-results/desktop-overview.png', fullPage: true })
  await page.getByRole('button', { name: '桌面授权', exact: true }).click()
  await page.screenshot({ path: 'test-results/desktop-settings.png', fullPage: true })
})

test('credential file export is plaintext and import replaces the vault after confirmation', async ({ page }) => {
  await login(page)
  await page.getByRole('button', { name: '凭据', exact: true }).click()
  await expect(page.getByText('未加入', { exact: true })).toBeVisible()
  const file = {
    format: 'localshelld-credentials',
    version: 1,
    credentials: { FILE_TEST: 'fixture-export-中文\nvalue' },
  }
  page.once('dialog', (dialog) => {
    expect(dialog.message()).toContain('替换全部')
    return dialog.accept()
  })
  await page.getByLabel('选择凭据文件').setInputFiles({
    name: 'credentials.json',
    mimeType: 'application/json',
    buffer: Buffer.from(JSON.stringify(file)),
  })
  await expect(page.getByText('凭据已导入。', { exact: true })).toBeVisible()
  await expect(page.getByRole('listitem').filter({ hasText: 'FILE_TEST' })).toBeVisible()
  page.once('dialog', (dialog) => {
    expect(dialog.message()).toContain('明文凭据')
    return dialog.accept()
  })
  const download = page.waitForEvent('download')
  await page.getByRole('button', { name: '导出文件', exact: true }).click()
  const downloaded = await download
  expect(JSON.parse(await readFile((await downloaded.path())!, 'utf8'))).toEqual(file)
  const invalid = { ...file, credentials: { lower: 'invalid' } }
  page.once('dialog', (dialog) => dialog.accept())
  await page
    .getByLabel('选择凭据文件')
    .setInputFiles({ name: 'invalid.json', mimeType: 'application/json', buffer: Buffer.from(JSON.stringify(invalid)) })
  await expect(page.getByRole('alert')).toContainText('key must match')
  await expect(page.getByRole('listitem').filter({ hasText: 'FILE_TEST' })).toBeVisible()
  page.once('dialog', (dialog) => dialog.accept())
  await page
    .getByRole('listitem')
    .filter({ hasText: 'FILE_TEST' })
    .getByRole('button', { name: '删除', exact: true })
    .click()
  await expect(page.getByText('凭据已删除。', { exact: true })).toBeVisible()
})
