import { type FormEvent, useEffect, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../../../../../Feedback'
import { api, dateTime } from '../../../../../api'
import { useAction } from '../../../../../useAction'

import './index.scss'

type Status = { url: string | null; revision: number | null; last_sync: number | null; error: string | null }
type Pending = { verification_uri: string; user_code: string; expires_in: number; interval: number }

export function SyncSettings({ session, onChanged }: { session: string; onChanged: () => Promise<void> }) {
  const [status, setStatus] = useState<Status | null>(null)
  const [url, setUrl] = useState('')
  const [name, setName] = useState('我的电脑')
  const [pending, setPending] = useState<Pending | null>(null)
  const action = useAction()

  async function refresh() {
    const status = await api<Status>('/v1/admin/sync', { action: 'status' }, session)
    setStatus(status)
    return status
  }

  useEffect(() => {
    void action.run(async () => {
      const status = await refresh()
      setUrl(status.url ?? '')
    })
    const timer = window.setInterval(() => {
      void refresh().catch(() => {})
    }, 5000)
    return () => window.clearInterval(timer)
  }, [session])

  useEffect(() => {
    if (!pending) return
    const timeout = window.setTimeout(() => setPending(null), pending.expires_in * 1000)
    const timer = window.setInterval(
      () => {
        void action.run(async () => {
          const result = await api<{ connected?: boolean }>('/v1/admin/sync', { action: 'poll' }, session)
          if (result.connected) {
            setPending(null)
            await refresh()
            await onChanged()
          }
        })
      },
      Math.max(3, pending.interval) * 1000
    )
    return () => {
      window.clearInterval(timer)
      window.clearTimeout(timeout)
    }
  }, [pending, session])

  function connect(event: FormEvent) {
    event.preventDefault()
    if (
      !window.confirm(
        '加入后，后端的凭据将覆盖本机全部凭据，本机也会与同一机主的其他设备互相信任。请先导出需要保留的本机凭据。继续加入？'
      )
    )
      return
    void action.run(async () => {
      setPending(await api<Pending>('/v1/admin/sync', { action: 'start', url, name }, session))
    })
  }

  function disconnect() {
    if (!window.confirm('离开后端？本机保留最后同步的凭据，但不再同步，也不再与其他设备互通。')) return
    void action.run(async () => {
      await api('/v1/admin/sync', { action: 'disconnect' }, session)
      setPending(null)
      await refresh()
      await onChanged()
    }, '已离开后端。')
  }

  function syncNow() {
    void action.run(async () => {
      await api('/v1/admin/sync', { action: 'refresh' }, session)
      await refresh()
      await onChanged()
    }, '已同步。')
  }

  return (
    <section className="sync-settings">
      <div className="sync-settings-heading">
        <h3 className="subsection-title">后端</h3>
        <span className="sync-settings-mode">{status ? (status.url ? '已加入' : '未加入') : '正在读取'}</span>
      </div>
      {status?.url && (
        <div className="sync-settings-status">
          <a className="content-link" href={status.url} target="_blank" rel="noreferrer">
            {status.url}
          </a>
          <span className="sync-settings-time">
            版本 {status.revision}
            {status.last_sync ? `，同步于 ${dateTime(status.last_sync)}` : ''}
          </span>
          {status.error && (
            <p className="sync-settings-error" role="alert">
              {status.error}
            </p>
          )}
          <div className="form-actions">
            <Button onClick={syncNow} disabled={action.busy}>
              立即同步
            </Button>
            <Button variant="ghost" onClick={disconnect} disabled={action.busy}>
              离开后端
            </Button>
          </div>
        </div>
      )}
      <details className="sync-settings-connect" open={!!pending || !!status?.error}>
        <summary className="sync-settings-summary">{status?.url ? '重新加入后端' : '加入后端'}</summary>
        <form className="form-stack" onSubmit={connect}>
          <Field htmlFor="sync-url" label="后端地址">
            <TextBox
              id="sync-url"
              type="url"
              value={url}
              onChange={(event) => setUrl(event.currentTarget.value)}
              placeholder="https://trustd.example.com"
              autoComplete="off"
              required
            />
          </Field>
          <Field htmlFor="sync-device-name" label="设备名称">
            <TextBox
              id="sync-device-name"
              value={name}
              onChange={(event) => setName(event.currentTarget.value)}
              maxLength={80}
              required
            />
          </Field>
          <div className="form-actions">
            <Button type="submit" disabled={action.busy}>
              {pending ? '重新请求加入' : '加入后端'}
            </Button>
          </div>
        </form>
        {pending && (
          <div className="sync-settings-pending">
            <span className="sync-settings-code">{pending.user_code}</span>
            <a className="content-link" href={pending.verification_uri} target="_blank" rel="noreferrer">
              打开后端，用通行密钥批准此设备
            </a>
            <span className="sync-settings-time">正在等待批准，只需按一次通行密钥。</span>
          </div>
        )}
      </details>
      <Feedback {...action} />
    </section>
  )
}
