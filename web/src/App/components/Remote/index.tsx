import { useEffect, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { Host } from './components/Host'

import { Feedback } from '../../../Feedback'
import { api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

type Peer = { label: string; public_key: string }
type Status = {
  device_id: string
  public_key: string
  joined: boolean
  backend: string | null
  connected: boolean
  peers: Record<string, Peer>
}

export function Remote({ session }: { session: string }) {
  const [status, setStatus] = useState<Status | null>(null)
  const [target, setTarget] = useState('')
  const action = useAction()

  async function refresh() {
    setStatus(await api<Status>('/v2/admin/remote', undefined, session))
  }
  useEffect(() => {
    void action.run(refresh)
    const timer = window.setInterval(() => void refresh().catch(() => {}), 5000)
    return () => window.clearInterval(timer)
  }, [session])

  const peers = Object.entries(status?.peers ?? {})
  return (
    <section className="remote-view">
      <div className="remote-heading">
        <h2 className="section-title">远端主机</h2>
        <span className="remote-connection">{status?.connected ? '已连接后端' : '未连接后端'}</span>
      </div>
      <Feedback {...action} />
      {status && !status.joined && (
        <p className="description remote-join-hint">
          本机尚未加入后端。在“凭据”中加入后端后，同一主人的其他设备会自动出现在这里，彼此默认信任。
        </p>
      )}
      {status?.joined && !peers.length && <p className="empty-state">同一主人的其他设备加入后端后会自动出现在这里。</p>}
      <div className="remote-peers">
        {peers.map(([id, peer]) => (
          <article className="remote-peer" key={id}>
            <div className="remote-peer-identity">
              <strong>{peer.label}</strong>
              <code className="remote-peer-id">{id}</code>
            </div>
            <div className="remote-actions">
              <Button onClick={() => setTarget(id)}>连接</Button>
            </div>
          </article>
        ))}
      </div>
      {status && (
        <details className="remote-setup">
          <summary>本机身份</summary>
          <Field label="本机设备 ID" htmlFor="remote-id">
            <TextBox id="remote-id" value={status.device_id} readOnly />
          </Field>
          <Field label="后端" htmlFor="remote-backend">
            <TextBox id="remote-backend" value={status.backend ?? ''} readOnly />
          </Field>
        </details>
      )}
      {target && <Host key={target} target={target} label={status?.peers[target]?.label ?? target} session={session} />}
    </section>
  )
}
