import { type Page, expect, test } from '@playwright/test'

async function setup(
  page: Page,
  registered = true,
  systemInitialized: boolean | null = registered,
  shellAvailable = true,
  powerShellFails = false
) {
  const info = {
    version: '0.1.0',
    platform: 'windows',
    program: 'C:\\Program Files\\localshelld',
    system_config: 'C:\\ProgramData\\localshelld\\system\\config.toml',
    user_config: 'C:\\Users\\Example\\AppData\\Roaming\\localshelld\\user.toml',
    registered,
    registered_executable: registered ? 'D:\\bin\\localshelld.exe' : null,
    registered_config: registered ? 'D:\\old\\config.toml' : null,
    system_running: registered,
    system_initialized: systemInitialized,
    user_initialized: registered,
    shell: 'C:\\Program Files\\PowerShell\\7\\pwsh.exe',
    shell_available: shellAvailable,
    powershell_msix: shellAvailable ? [] : ['Microsoft.PowerShell'],
    elevation_available: true,
    user_url: 'http://localhost:32120/',
    system_url: 'http://localhost:32119/',
    joined: registered,
    backend: registered ? 'https://backend.example' : null,
    device_name: 'fixture-pc',
    agents: [
      { id: 'codex', name: 'Codex', path: 'C:\\Users\\Example\\.codex\\config.toml', detected: true },
      { id: 'cursor', name: 'Cursor', path: 'C:\\Users\\Example\\.cursor\\mcp.json', detected: false },
    ],
    job: {
      running: false,
      stage: '',
      error: null as string | null,
      completed: null as string | null,
      agents: [] as { name: string; path: string; backup: string | null; error: string | null }[],
      join: null as { uri: string; code: string } | null,
    },
  }
  // Status polls that still report a pending join before the owner approves.
  let joinPolls = 0
  const installs: unknown[] = []
  await page.route('**/setup/v1/**', async (route) => {
    expect(route.request().headers().authorization).toBe('Bearer browser-fixture')
    const path = new URL(route.request().url()).pathname
    if (path.endsWith('/powershell')) {
      expect(route.request().postDataJSON()).toEqual({ consent: true })
      installs.push({ consent: true })
      info.shell_available = !powerShellFails
      info.job = {
        running: false,
        stage: powerShellFails ? 'failed' : 'done',
        error: powerShellFails ? '系统授权已取消。' : null,
        completed: powerShellFails ? null : 'powershell',
        agents: [],
        join: null,
      }
    }
    if (path.endsWith('/install')) {
      const body = route.request().postDataJSON()
      installs.push(body)
      if (body.backend) joinPolls = 2
      info.job = {
        running: false,
        stage: 'done',
        error: null,
        completed: 'install',
        join: null,
        agents: info.agents
          .filter((agent) => body.agents.includes(agent.id))
          .map((agent) => ({
            name: agent.name,
            path: agent.path,
            backup: body.replace_agents ? `${agent.path}.trustd-backup-fixture` : null,
            error: agent.id === 'codex' && !body.replace_agents ? '已有 trustd 配置；勾选替换后重试。' : null,
          })),
      }
    }
    if (path.endsWith('/status') && joinPolls > 0) {
      joinPolls -= 1
      const job = { ...info.job, running: true, stage: 'join', completed: null }
      job.join = { uri: 'https://backend.example/#join=ABC123', code: 'ABC123' }
      await route.fulfill({ json: { ...info, job } })
      return
    }
    await route.fulfill({ json: path.endsWith('/status') ? info : { started: true } })
  })
  await page.goto('/maintenance.html#browser-fixture')
  await expect(page.getByRole('button', { name: '下一步' })).toBeVisible()
  return installs
}

test('PowerShell installation asks consent, returns to checks and guides MSIX removal', async ({ page }) => {
  const installs = await setup(page, false, false, false)
  await expect(page.getByRole('button', { name: '下一步' })).toBeDisabled()
  await expect(page.getByText('trustd 不兼容 MSIX 版 PowerShell，需要安装系统级版本。')).toBeVisible()
  page.once('dialog', (dialog) => dialog.dismiss())
  await page.getByRole('button', { name: '安装 PowerShell 7', exact: true }).click()
  expect(installs).toEqual([])
  page.once('dialog', (dialog) => dialog.accept())
  await page.getByRole('button', { name: '安装 PowerShell 7', exact: true }).click()
  await expect(page.getByRole('button', { name: '下一步' })).toBeEnabled()
  expect(installs).toEqual([{ consent: true }])
  await expect(
    page.getByText('Get-AppxPackage -Name Microsoft.PowerShell | Remove-AppxPackage', { exact: true })
  ).toBeVisible()
  await expect(page.getByText('系统服务和用户服务已启动。')).toHaveCount(0)
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/maintenance-powershell.png', fullPage: true })
  await page.getByRole('button', { name: '下一步' }).click()
  await page.getByLabel('后端地址').fill('https://backend.example')
  await page.getByRole('button', { name: '开始安装' }).click()
  await expect(page.getByRole('heading', { name: '安装完成' })).toBeVisible({ timeout: 15000 })
  expect(installs).toEqual([
    { consent: true },
    { backend: 'https://backend.example', device_name: 'fixture-pc', agents: [], replace_agents: false },
  ])
})

test('PowerShell authorization failure allows a fresh attempt without enabling trustd installation', async ({
  page,
}) => {
  const installs = await setup(page, false, false, false, true)
  page.once('dialog', (dialog) => dialog.accept())
  await page.getByRole('button', { name: '安装 PowerShell 7', exact: true }).click()
  await expect(page.getByText('系统授权已取消。')).toBeVisible()
  await page.getByRole('button', { name: '返回检查' }).click()
  await expect(page.getByRole('button', { name: '下一步' })).toBeDisabled()
  await expect(page.getByRole('button', { name: '安装 PowerShell 7', exact: true })).toBeEnabled()
  expect(installs).toEqual([{ consent: true }])
})

test('maintenance checks locations, preserves credentials and finishes without real elevation', async ({ page }) => {
  const installs = await setup(page)
  await expect(page.getByRole('heading', { name: '已检测到安装' })).toBeVisible()
  await expect(page.getByText('C:\\Program Files\\localshelld', { exact: true })).toBeVisible()
  await page.screenshot({ path: 'test-results/maintenance-desktop.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/maintenance-mobile.png', fullPage: true })
  await page.getByRole('button', { name: '下一步' }).click()
  await expect(page.getByLabel('后端地址')).toHaveCount(0)
  await expect(page.getByText('此设备已加入 https://backend.example', { exact: false })).toBeVisible()
  await page.getByRole('button', { name: '安装并修复' }).click()
  await expect(page.getByRole('heading', { name: '安装完成' })).toBeVisible()
  expect(installs).toEqual([{ backend: null, device_name: null, agents: [], replace_agents: false }])
  // The paired system daemon has no management page of its own.
  await expect(page.getByRole('link', { name: '打开系统管理' })).toHaveCount(0)
  await expect(page.getByRole('link', { name: '打开管理页' })).toHaveAttribute('href', 'http://localhost:32120/')
  await page.getByRole('button', { name: '完成', exact: true }).click()
  await expect(page.getByRole('heading', { name: '向导已关闭' })).toBeVisible()
})

test('a new device must name its backend and shows the approval link while joining', async ({ page }) => {
  const installs = await setup(page, false)
  await page.getByRole('button', { name: '下一步' }).click()
  await expect(page.getByLabel('主密码', { exact: true })).toHaveCount(0)
  await page.getByRole('button', { name: '开始安装' }).click()
  // The required backend address blocks submission.
  expect(installs).toEqual([])
  await page.getByLabel('后端地址').fill('https://backend.example')
  await page.getByLabel('设备名称').fill('Office laptop')
  await page.getByRole('button', { name: '开始安装' }).click()
  await expect(page.locator('.maintenance-join-code')).toHaveText('ABC123')
  await expect(page.getByRole('link', { name: '打开后端，用通行密钥批准此设备' })).toHaveAttribute(
    'href',
    'https://backend.example/#join=ABC123'
  )
  await page.screenshot({ path: 'test-results/maintenance-join.png', fullPage: true })
  await expect(page.getByRole('heading', { name: '安装完成' })).toBeVisible({ timeout: 15000 })
  expect(installs).toEqual([
    { backend: 'https://backend.example', device_name: 'Office laptop', agents: [], replace_agents: false },
  ])
})

test('a direct page without the launch capability cannot offer installation', async ({ page }) => {
  await page.goto('/maintenance.html')
  await expect(page.getByRole('heading', { name: '重新打开向导' })).toBeVisible()
  await expect(page.getByRole('button', { name: '下一步' })).toHaveCount(0)
})

test('agent selection is opt-in, retains choices across steps and reports partial failure', async ({ page }) => {
  const installs = await setup(page)
  await page.getByRole('button', { name: '下一步' }).click()
  await expect(page.getByRole('checkbox', { name: /Codex/ })).not.toBeChecked()
  await expect(page.getByRole('checkbox', { name: /Cursor/ })).not.toBeChecked()
  await page.getByRole('checkbox', { name: /Codex/ }).check()
  await page.getByRole('checkbox', { name: /Cursor/ }).check()
  await page.screenshot({ path: 'test-results/maintenance-agents-desktop.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/maintenance-agents-mobile.png', fullPage: true })
  await page.getByRole('button', { name: '上一步' }).click()
  await page.getByRole('button', { name: '下一步' }).click()
  await expect(page.getByRole('checkbox', { name: /Codex/ })).toBeChecked()
  await page.getByRole('button', { name: '安装并修复' }).click()
  await expect(page.getByRole('heading', { name: '安装完成' })).toBeVisible()
  await expect(page.getByText('已有 trustd 配置；勾选替换后重试。')).toBeVisible()
  await expect(page.getByText('已配置', { exact: true })).toHaveCount(1)
  expect(installs).toEqual([{ backend: null, device_name: null, agents: ['codex', 'cursor'], replace_agents: false }])
})

test('replacement requires an explicit checkbox and displays its backup', async ({ page }) => {
  const installs = await setup(page)
  await page.getByRole('button', { name: '下一步' }).click()
  await page.getByRole('checkbox', { name: /Codex/ }).check()
  await page.getByRole('checkbox', { name: /替换所选客户端/ }).check()
  await page.getByRole('button', { name: '安装并修复' }).click()
  await expect(page.getByRole('heading', { name: '安装完成' })).toBeVisible()
  await page.getByText('配置文件与备份', { exact: true }).click()
  await expect(page.getByText(/备份：.*trustd-backup-fixture/)).toBeVisible()
  expect(installs).toEqual([{ backend: null, device_name: null, agents: ['codex'], replace_agents: true }])
})
