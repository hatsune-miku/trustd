import { useEffect, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { Host } from './components/Host'

import { Feedback } from '../../../Feedback'
import { api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

type Peer = { label: string; public_key: string; allow_execution: boolean; sudo: boolean; secret_keys: string[] }
type Status = {
  device_id: string
  public_key: string
  relay_url: string
  token_configured: boolean
  connected: boolean
  peers: Record<string, Peer>
}

export function Remote({ session }: { session: string }) {
  const [status, setStatus] = useState<Status | null>(null)
  const [relay, setRelay] = useState('')
  const [token, setToken] = useState('')
  const [label, setLabel] = useState('')
  const [publicKey, setPublicKey] = useState('')
  const [execution, setExecution] = useState(false)
  const [sudo, setSudo] = useState(false)
  const [keys, setKeys] = useState('')
  const [target, setTarget] = useState('')
  const action = useAction()

  async function refresh(initial = false) {
    const result = await api<Status>('/v2/admin/remote', undefined, session)
    setStatus(result)
    if (initial) setRelay(result.relay_url)
  }
  useEffect(() => {
    void action.run(() => refresh(true))
    const timer = window.setInterval(() => void refresh().catch(() => {}), 5000)
    return () => window.clearInterval(timer)
  }, [session])

  async function saveConnection() {
    await api('/v2/admin/remote', { relay_url: relay, relay_token: token }, session)
    setToken('')
    await refresh()
  }
  async function savePeer() {
    await api(
      '/v2/admin/remote/peer',
      {
        label,
        public_key: publicKey,
        allow_execution: execution,
        sudo,
        secret_keys: keys.split(/[\s,]+/).filter(Boolean),
      },
      session
    )
    setLabel('')
    setPublicKey('')
    setKeys('')
    setSudo(false)
    setExecution(false)
    await refresh()
  }
  function edit(peer: Peer) {
    setLabel(peer.label)
    setPublicKey(peer.public_key)
    setExecution(peer.allow_execution)
    setSudo(peer.sudo)
    setKeys(peer.secret_keys.join(', '))
  }
  async function remove(id: string) {
    await api('/v2/admin/remote/peer', { device_id: id, remove: true }, session)
    if (target === id) setTarget('')
    await refresh()
  }

  return (
    <section className="remote-view">
      <div className="remote-heading">
        <h2 className="section-title">远端主机</h2>
        <span className="remote-connection">{status?.connected ? '中转已连接' : '中转未连接'}</span>
      </div>
      <Feedback {...action} />
      <details className="remote-setup" open={!status?.relay_url}>
        <summary>连接设置</summary>
        <form
          className="form-stack"
          onSubmit={(event) => {
            event.preventDefault()
            void action.run(saveConnection, '连接设置已保存。')
          }}
        >
          <Field label="中转地址" htmlFor="remote-relay">
            <TextBox
              id="remote-relay"
              value={relay}
              onChange={(event) => setRelay(event.target.value)}
              placeholder="https://relay.example.com"
              required
            />
          </Field>
          <Field label="设备接入令牌" htmlFor="remote-token">
            <TextBox
              id="remote-token"
              type="password"
              value={token}
              onChange={(event) => setToken(event.target.value)}
              placeholder={status?.token_configured ? '留空保留现有令牌' : ''}
              autoComplete="off"
            />
          </Field>
          <Button type="submit" disabled={action.busy}>
            保存连接
          </Button>
        </form>
        <Field label="本机设备 ID" htmlFor="remote-id">
          <TextBox id="remote-id" value={status?.device_id ?? ''} readOnly />
        </Field>
        <Field label="本机公钥" htmlFor="remote-public">
          <TextBox id="remote-public" value={status?.public_key ?? ''} readOnly />
        </Field>
      </details>
      <div className="remote-peers">
        {Object.entries(status?.peers ?? {}).map(([id, peer]) => (
          <article className="remote-peer" key={id}>
            <div className="remote-peer-identity">
              <strong>{peer.label}</strong>
              <code className="remote-peer-id">{id}</code>
              <span>{peer.allow_execution ? '允许此设备在本机执行' : '未授予本机执行权限'}</span>
            </div>
            <div className="remote-actions">
              <Button onClick={() => setTarget(id)}>连接</Button>
              <Button variant="ghost" onClick={() => edit(peer)}>
                编辑
              </Button>
              <Button variant="ghost" onClick={() => void action.run(() => remove(id))}>
                移除
              </Button>
            </div>
          </article>
        ))}
      </div>
      <details className="remote-setup" open={Boolean(publicKey)}>
        <summary>添加或编辑设备</summary>
        <form
          className="form-stack"
          onSubmit={(event) => {
            event.preventDefault()
            void action.run(savePeer, '设备已保存。')
          }}
        >
          <Field label="名称" htmlFor="remote-label">
            <TextBox id="remote-label" value={label} onChange={(event) => setLabel(event.target.value)} required />
          </Field>
          <Field label="对方公钥" htmlFor="remote-peer-key">
            <TextBox
              id="remote-peer-key"
              value={publicKey}
              onChange={(event) => setPublicKey(event.target.value)}
              required
            />
          </Field>
          <label className="remote-check">
            <input
              className="remote-checkbox"
              type="checkbox"
              checked={execution}
              onChange={(event) => setExecution(event.target.checked)}
            />
            允许此设备在本机执行
          </label>
          <label className="remote-check">
            <input
              className="remote-checkbox"
              type="checkbox"
              checked={sudo}
              onChange={(event) => setSudo(event.target.checked)}
            />
            允许请求管理员 / root 身份
          </label>
          <Field label="允许使用的凭据" htmlFor="remote-keys">
            <TextBox
              id="remote-keys"
              value={keys}
              onChange={(event) => setKeys(event.target.value)}
              placeholder="凭据名称，以逗号分隔"
            />
          </Field>
          <Button type="submit" disabled={action.busy}>
            保存设备
          </Button>
        </form>
      </details>
      {target && <Host key={target} target={target} label={status?.peers[target]?.label ?? target} session={session} />}
    </section>
  )
}
