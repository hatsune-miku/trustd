import { Button } from '@a1knla/cakeui'

import './index.scss'

type Props = { available: boolean; msix: string[] | null; busy: boolean; onInstall: () => void }

export function PowerShellSetup({ available, msix, busy, onInstall }: Props) {
  if (available && msix?.length === 0) return null
  return (
    <section className="powershell-setup" aria-label="PowerShell 安装">
      {!available ? (
        <>
          <p className="powershell-setup-note">trustd 不兼容 MSIX 版 PowerShell，需要安装系统级版本。</p>
          <Button onClick={onInstall} disabled={busy}>
            安装 PowerShell 7
          </Button>
        </>
      ) : msix === null ? (
        <p className="powershell-setup-note">
          无法检查 MSIX 版 PowerShell。若曾通过商店或 WinGet 安装，请检查并卸载 MSIX 版，保留系统级版本。
        </p>
      ) : (
        <>
          <p className="powershell-setup-note">
            系统级 PowerShell 已就绪。trustd 不兼容 MSIX 版；为避免两个版本并存，请在 PowerShell
            中运行以下命令卸载检测到的 MSIX 版，再刷新检查。
          </p>
          {(msix ?? []).map((name) => (
            <code className="powershell-setup-command" key={name}>
              Get-AppxPackage -Name {name} | Remove-AppxPackage
            </code>
          ))}
        </>
      )}
    </section>
  )
}
