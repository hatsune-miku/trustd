import { type FormEvent, useEffect, useState } from 'react'

import { Button, ComboBox, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../../../Feedback'
import { type LoginResult, api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

type PasskeyLogin = {
  login_id: string
  verification_uri: string
  user_code: string
  expires_in: number
  interval: number
}

function ticket() {
  return new URLSearchParams(location.hash.slice(1)).get('ticket') ?? ''
}

/** The user daemon signs in with the owner's passkey on the backend. */
function OwnerPasskey({ onLogin }: { onLogin: (login: LoginResult) => void }) {
  const [pending, setPending] = useState<PasskeyLogin | null>(null)
  const action = useAction()
  useEffect(() => {
    if (!pending) return
    const expires = window.setTimeout(() => setPending(null), pending.expires_in * 1000)
    const timer = window.setInterval(
      () =>
        void action.run(async () => {
          const result = await api<LoginResult & { pending?: boolean }>('/v1/admin/login/passkey/poll', {
            login_id: pending.login_id,
          })
          if (!result.pending) {
            setPending(null)
            onLogin(result)
          }
        }),
      Math.max(2, pending.interval) * 1000
    )
    return () => {
      window.clearInterval(timer)
      window.clearTimeout(expires)
    }
  }, [pending])
  function start() {
    // Open the tab synchronously so the browser does not block it as a popup.
    const tab = window.open('', '_blank')
    void action.run(async () => {
      try {
        const started = await api<PasskeyLogin>('/v1/admin/login/passkey/start', {})
        setPending(started)
        if (tab) tab.location.href = started.verification_uri
      } catch (error) {
        tab?.close()
        throw error
      }
    })
  }
  return (
    <div className="form-stack">
      <p className="description">在后端页面按一次通行密钥即可登录。本机会自行验证签名。</p>
      <div className="form-actions">
        <Button variant="primary" onClick={start} loading={action.busy && !pending}>
          使用通行密钥登录
        </Button>
      </div>
      {pending && (
        <div className="login-pending">
          <span className="login-code">{pending.user_code}</span>
          <a className="content-link" href={pending.verification_uri} target="_blank" rel="noreferrer">
            打开后端确认登录
          </a>
          <span className="description">正在等待确认。</span>
        </div>
      )}
      <p className="description login-fallback">
        后端无法访问时，在本机终端运行 <code>trustd login-url</code> 获取一次性登录链接。
      </p>
      <Feedback {...action} />
    </div>
  )
}

/** An unpaired system daemon (system-only installation) keeps its own password. */
function Password({ onLogin }: { onLogin: (login: LoginResult) => void }) {
  const [kind, setKind] = useState('password')
  const [value, setValue] = useState('')
  const action = useAction()
  function submit(event: FormEvent) {
    event.preventDefault()
    void action.run(async () =>
      onLogin(await api<LoginResult>('/v1/admin/login', { credential: { type: kind, value } }))
    )
  }
  return (
    <form className="form-stack" onSubmit={submit}>
      <Field htmlFor="login-kind" label="验证方式">
        <ComboBox id="login-kind" value={kind} onChange={(event) => setKind(event.currentTarget.value)}>
          <option value="password">Master Password</option>
          <option value="totp">Authenticator 动态码</option>
        </ComboBox>
      </Field>
      <Field htmlFor="login-value" label={kind === 'totp' ? '6 位动态码' : 'Master Password'}>
        <TextBox
          id="login-value"
          type="text"
          value={value}
          onChange={(event) => setValue(event.currentTarget.value)}
          autoComplete="off"
          inputMode={kind === 'totp' ? 'numeric' : 'text'}
          required={kind === 'totp'}
        />
      </Field>
      <div className="form-actions">
        <Button type="submit" variant="primary" loading={action.busy}>
          验证身份
        </Button>
      </div>
      <Feedback {...action} />
    </form>
  )
}

export function Login({
  onLogin,
  scope,
  daemon,
}: {
  onLogin: (login: LoginResult) => void
  scope: string
  daemon: 'user' | 'privileged' | undefined
}) {
  const action = useAction()
  useEffect(() => {
    const value = ticket()
    if (!value) return
    history.replaceState(null, '', location.pathname)
    void action.run(async () =>
      onLogin(await api<LoginResult>('/v1/admin/login', { credential: { type: 'ticket', value } }))
    )
  }, [])
  return (
    <main className="login-view">
      <h2 className="section-title">验证后管理授权</h2>
      <p className="description">此入口管理{scope}权限。尊重用户的个人隐私。</p>
      <Feedback {...action} />
      {daemon === 'user' && <OwnerPasskey onLogin={onLogin} />}
      {daemon === 'privileged' && <Password onLogin={onLogin} />}
    </main>
  )
}
