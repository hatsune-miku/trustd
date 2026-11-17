import { useEffect, useState } from 'react'

import { TelegramSettings } from './components/TelegramSettings'

import { Feedback } from '../../../Feedback'
import { type Settings, api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

export function DesktopSettings({ session }: { session: string }) {
  const [settings, setSettings] = useState<Settings | null>(null)
  const action = useAction()
  useEffect(() => {
    void action.run(async () => setSettings(await api<Settings>('/v1/admin/settings', undefined, session)))
  }, [session])
  return (
    <div className="desktop-settings">
      <h2 className="section-title">桌面授权</h2>
      <p className="description">代理可请求本机弹窗授权。接受、拒绝由用户选择，忽略会等待至 600 秒超时。</p>
      <Feedback {...action} />
      <section className="desktop-section">
        <h3 className="subsection-title">本机进程配对</h3>
        {settings?.daemon === 'user' && !settings.desktop_available && (
          <p className="desktop-warning">
            此用户进程没有图形桌面。请在登录桌面中启动用户 daemon；无头模式无法显示授权弹窗。
          </p>
        )}
        {settings?.paired_peer ? (
          <p className="description pairing-status">
            安装程序已自动完成配对：{settings.paired_peer}。管理员 / root
            命令的授权也在这里确认，相关记录可在“日志”中切换查看。
          </p>
        ) : (
          settings && (
            <p className="description pairing-status">
              尚未配对。以管理员身份重新运行安装（修复）会自动完成配对，已有配置和凭据保持不变。
            </p>
          )
        )}
      </section>
      <TelegramSettings session={session} />
    </div>
  )
}
