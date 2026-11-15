import { useEffect, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { Codex } from './components/Codex'
import { Files } from './components/Files'
import { Jobs } from './components/Jobs'
import { RemoteLogs } from './components/RemoteLogs'

import { Feedback } from '../../../../../Feedback'
import { api } from '../../../../../api'
import { useAction } from '../../../../../useAction'

import './index.scss'

export type Invoke = <T>(operation: string, args?: Record<string, unknown>) => Promise<T>
type Info = { platform: string; arch: string; version: string; identities: Record<string, unknown> }
type Opened = { session_ref: string; privilege_levels: string[] }

export function Host({ target, label, session }: { target: string; label: string; session: string }) {
  const [info, setInfo] = useState<Info | null>(null)
  const [opened, setOpened] = useState<Opened | null>(null)
  const [cwd, setCwd] = useState('')
  const [keys, setKeys] = useState('')
  const [sudo, setSudo] = useState(false)
  const [identity, setIdentity] = useState('current-user')
  const [section, setSection] = useState('jobs')
  const action = useAction()
  async function invoke<T>(operation: string, args: Record<string, unknown> = {}): Promise<T> {
    return api<T>('/v2/admin/remote/invoke', { target, operation, arguments: args }, session)
  }
  useEffect(() => {
    void action.run(async () => setInfo(await invoke<Info>('host_info')))
  }, [target, session])
  async function open() {
    const result = await invoke<Opened>('session_open', {
      cwd: cwd || null,
      sudo,
      secret_keys: keys.split(/[\s,]+/).filter(Boolean),
    })
    setOpened(result)
    setIdentity('current-user')
  }
  async function close() {
    if (opened) await invoke('session_close', { session_ref: opened.session_ref })
    setOpened(null)
  }
  const execution = info?.platform === 'windows' ? { privilege_level: identity } : { sudo: identity !== 'current-user' }

  return (
    <section className="remote-host">
      <div className="remote-actions">
        <h3 className="remote-host-title">{label}</h3>
        {info && (
          <span>
            {info.platform} · {info.arch} · {info.version}
          </span>
        )}
        <Button variant="ghost" onClick={() => void action.run(async () => setInfo(await invoke<Info>('host_info')))}>
          刷新
        </Button>
      </div>
      <Feedback {...action} />
      {!opened ? (
        <form
          className="form-stack"
          onSubmit={(event) => {
            event.preventDefault()
            void action.run(open)
          }}
        >
          <Field label="初始目录" htmlFor="remote-cwd">
            <TextBox
              id="remote-cwd"
              value={cwd}
              onChange={(event) => setCwd(event.target.value)}
              placeholder="留空使用远端用户目录"
            />
          </Field>
          <Field label="本次使用的凭据" htmlFor="remote-session-keys">
            <TextBox
              id="remote-session-keys"
              value={keys}
              onChange={(event) => setKeys(event.target.value)}
              placeholder="凭据名称，以逗号分隔"
            />
          </Field>
          <label className="remote-check">
            <input
              className="remote-checkbox"
              type="checkbox"
              checked={sudo}
              onChange={(event) => setSudo(event.target.checked)}
            />
            申请管理员 / root 身份
          </label>
          <Button type="submit" disabled={action.busy || !info}>
            进入远端会话
          </Button>
        </form>
      ) : (
        <div className="remote-actions">
          <label>
            执行身份{' '}
            <select className="remote-select" value={identity} onChange={(event) => setIdentity(event.target.value)}>
              {opened.privilege_levels.map((level) => (
                <option key={level} value={level}>
                  {level}
                </option>
              ))}
            </select>
          </label>
          <Button variant="ghost" onClick={() => void action.run(close)}>
            关闭会话
          </Button>
        </div>
      )}
      <nav className="remote-actions" aria-label="远端功能">
        {[
          ['jobs', '任务'],
          ['codex', 'Codex'],
          ['files', '文件'],
          ['logs', '日志'],
        ].map(([id, text]) => (
          <Button key={id} variant={section === id ? 'primary' : 'ghost'} onClick={() => setSection(id)}>
            {text}
          </Button>
        ))}
      </nav>
      {info && section === 'jobs' && <Jobs invoke={invoke} sessionRef={opened?.session_ref} execution={execution} />}
      {info && section === 'codex' && (
        <Codex invoke={invoke} sessionRef={opened?.session_ref} target={target} cwd={cwd} />
      )}
      {info && section === 'files' && <Files invoke={invoke} sessionRef={opened?.session_ref} />}
      {info && section === 'logs' && <RemoteLogs invoke={invoke} />}
    </section>
  )
}
