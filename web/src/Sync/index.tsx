import { type FormEvent, useEffect, useState } from 'react'

import { Button, CakeProvider, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../Feedback'
import { api, dateTime } from '../api'
import { useAction } from '../useAction'
import { type Assertion, type Creation, createCredential, getCredential } from '../webauthn'

import './index.scss'

type Info = { protocol: number; initialized: boolean; origin: string }
type Tokens = { access_token: string; expires_at: number }
/** Exact statement text plus what a trusted owner passkey needs to sign it. */
type Prepared = { statement: string; challenge: string; rp_id: string; allow_credentials: string[] }
type Registered = Tokens & { endorse?: Prepared }
type JoinRequest = Prepared & { name: string; device_id: string; user_code: string; approved: boolean }
type Passkey = { id: string; name: string; trusted: boolean }
type Device = { name: string; approved_at: number }
type Roster = { devices: Record<string, Device> }

function joinCode() {
  return new URLSearchParams(location.hash.slice(1)).get('join') ?? ''
}

/** Have the owner's passkey sign a prepared statement (one tap). */
function sign(prepared: Prepared) {
  const options: Assertion = {
    challenge: prepared.challenge,
    rpId: prepared.rp_id,
    allowCredentials: prepared.allow_credentials.map((id) => ({ type: 'public-key', id })),
    userVerification: 'required',
    timeout: 120000,
  }
  return getCredential(options)
}

export function Sync() {
  const [info, setInfo] = useState<Info | null>(null)
  const [login, setLogin] = useState<Tokens | null>(null)
  const [bootstrap, setBootstrap] = useState('')
  const [name, setName] = useState('我的通行密钥')
  const [code, setCode] = useState(joinCode)
  const [request, setRequest] = useState<JoinRequest | null>(null)
  const [passkeys, setPasskeys] = useState<Passkey[]>([])
  const [roster, setRoster] = useState<Roster | null>(null)
  const action = useAction()

  useEffect(() => {
    void action.run(async () => setInfo(await api<Info>('/sync/v1/info')))
    const changed = () => setCode(joinCode())
    window.addEventListener('hashchange', changed)
    return () => window.removeEventListener('hashchange', changed)
  }, [])
  useEffect(() => {
    setRequest(null)
    if (code)
      void action.run(async () => setRequest(await api<JoinRequest>('/sync/v1/device/inspect', { user_code: code })))
  }, [code])
  useEffect(() => {
    if (!login) return
    const timer = window.setTimeout(
      () => {
        setLogin(null)
        setRoster(null)
      },
      Math.max(0, login.expires_at * 1000 - Date.now())
    )
    return () => window.clearTimeout(timer)
  }, [login])

  async function refresh(token = login?.access_token) {
    if (!token) return
    setPasskeys((await api<{ passkeys: Passkey[] }>('/sync/v1/sessions', undefined, token)).passkeys)
    setRoster(await api<Roster>('/sync/v1/roster', undefined, token))
  }

  async function signedIn(result: Tokens) {
    setLogin(result)
    setBootstrap('')
    setInfo(await api<Info>('/sync/v1/info'))
    await refresh(result.access_token)
  }

  async function submit(prepared: Prepared) {
    await api('/sync/v1/statements', { statement: prepared.statement, assertion: await sign(prepared) })
  }

  function register(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      const challenge = await api<{ challenge_id: string; options: { publicKey: Creation } }>(
        '/sync/v1/passkeys/register/start',
        { name, bootstrap_code: bootstrap },
        login?.access_token
      )
      const result = await api<Registered>(
        '/sync/v1/passkeys/register/finish',
        { challenge_id: challenge.challenge_id, credential: await createCredential(challenge.options.publicKey) },
        login?.access_token
      )
      // A later passkey is trusted by devices only once an existing one endorses it.
      if (result.endorse) await submit(result.endorse)
      await signedIn(result)
    }, '通行密钥已添加。')
  }

  function signIn() {
    void action.run(async () => {
      const challenge = await api<{ challenge_id: string; options: { publicKey: Assertion } }>(
        '/sync/v1/passkeys/authenticate/start',
        {}
      )
      await signedIn(
        await api<Tokens>('/sync/v1/passkeys/authenticate/finish', {
          challenge_id: challenge.challenge_id,
          credential: await getCredential(challenge.options.publicKey),
        })
      )
    })
  }

  function approve() {
    if (!request) return
    void action.run(async () => {
      await api('/sync/v1/device/approve', { user_code: request.user_code, assertion: await sign(request) })
      setRequest({ ...request, approved: true })
      history.replaceState(null, '', location.pathname)
      await refresh()
    }, '设备已加入，可以回到该设备继续。')
  }

  function signed(kind: 'device_revoke' | 'passkey_add' | 'passkey_remove', body: object, done: string) {
    void action.run(async () => {
      await submit(await api<Prepared>('/sync/v1/statements/prepare', { kind, ...body }, login?.access_token))
      await refresh()
    }, done)
  }

  function logout() {
    void action.run(async () => {
      try {
        await api('/sync/v1/session/revoke', { id: '' }, login?.access_token)
      } finally {
        setLogin(null)
        setRoster(null)
      }
    })
  }

  const devices = Object.entries(roster?.devices ?? {})
  return (
    <CakeProvider mode="light" theme="blue" accentColor="#245aa8" className="app-theme">
      <main className="sync-page">
        <header className="sync-header">
          <h1 className="sync-title">trustd 后端</h1>
          <span className="sync-brand">trustd</span>
          {login && (
            <Button variant="ghost" onClick={logout} disabled={action.busy}>
              退出登录
            </Button>
          )}
        </header>
        <div className="sync-content">
          <Feedback {...action} />
          {!info && <p className="description">正在连接后端。</p>}
          {request && (
            <section className="sync-section sync-join">
              <h2 className="section-title">批准设备加入</h2>
              <div className="sync-device">
                <strong className="sync-device-name">{request.name}</strong>
                <span className="sync-device-code">{request.user_code}</span>
                <code className="sync-device-id">{request.device_id}</code>
                <p className="sync-device-hint">
                  核对该设备上显示的验证码。批准后，它与你的其他设备互相信任，并可读写全部凭据。
                </p>
                <Button variant="primary" onClick={approve} disabled={action.busy || request.approved}>
                  {request.approved ? '已批准' : '批准此设备'}
                </Button>
              </div>
            </section>
          )}
          {info?.initialized && !login && (
            <section className="sync-section">
              <h2 className="section-title">管理后端</h2>
              <Button variant="primary" onClick={signIn} loading={action.busy}>
                使用通行密钥登录
              </Button>
            </section>
          )}
          {info && (!info.initialized || login) && (
            <>
              {!info.initialized && <h2 className="section-title">初始化后端</h2>}
              {login && (
                <section className="sync-section">
                  <h2 className="section-title">我的设备</h2>
                  <ul className="sync-list">
                    {devices.map(([id, device]) => (
                      <li className="sync-row" key={id}>
                        <div className="sync-row-name">
                          <span className="sync-session-name">{device.name}</span>
                          <span className="sync-session-expiry">加入于 {dateTime(device.approved_at)}</span>
                        </div>
                        <Button
                          variant="ghost"
                          disabled={action.busy}
                          onClick={() => {
                            if (
                              !window.confirm(`撤销 ${device.name}？该设备将永久失去访问，需重置身份后才能重新加入。`)
                            )
                              return
                            signed('device_revoke', { device_id: id }, '设备已撤销。')
                          }}
                        >
                          撤销
                        </Button>
                      </li>
                    ))}
                  </ul>
                  {!devices.length && (
                    <p className="empty-state">尚无设备。在设备的管理页“凭据”中填写此后端地址即可加入。</p>
                  )}
                  <Button variant="ghost" disabled={action.busy} onClick={() => void action.run(() => refresh())}>
                    刷新设备
                  </Button>
                </section>
              )}
              <section className="sync-section">
                {login && <h2 className="section-title">通行密钥</h2>}
                <form className="form-stack" onSubmit={register}>
                  {!info.initialized && (
                    <Field
                      htmlFor="bootstrap-code"
                      label="初始化码"
                      description="位于服务器数据目录的 bootstrap-code 文件中。"
                    >
                      <TextBox
                        id="bootstrap-code"
                        value={bootstrap}
                        onChange={(event) => setBootstrap(event.currentTarget.value.trim())}
                        autoComplete="off"
                        spellCheck={false}
                        required
                      />
                    </Field>
                  )}
                  <Field htmlFor="passkey-name" label="通行密钥名称">
                    <TextBox
                      id="passkey-name"
                      value={name}
                      onChange={(event) => setName(event.currentTarget.value)}
                      maxLength={80}
                      required
                    />
                  </Field>
                  <div className="form-actions">
                    <Button type="submit" variant="primary" disabled={action.busy}>
                      {info.initialized ? '添加通行密钥' : '创建通行密钥并初始化'}
                    </Button>
                  </div>
                </form>
                <ul className="sync-list">
                  {passkeys.map((key) => (
                    <li className="sync-row" key={key.id}>
                      <span className="sync-row-name">
                        {key.name}
                        {!key.trusted && <span className="sync-session-expiry">（设备尚未信任）</span>}
                      </span>
                      {!key.trusted && (
                        <Button
                          variant="ghost"
                          disabled={action.busy}
                          onClick={() => signed('passkey_add', { id: key.id }, '设备已信任此通行密钥。')}
                        >
                          确认信任
                        </Button>
                      )}
                      <Button
                        variant="ghost"
                        disabled={action.busy || passkeys.length === 1}
                        onClick={() => {
                          if (!window.confirm(`移除通行密钥 ${key.name}？`)) return
                          signed('passkey_remove', { id: key.id }, '通行密钥已移除。')
                        }}
                      >
                        移除
                      </Button>
                    </li>
                  ))}
                </ul>
              </section>
            </>
          )}
        </div>
        <footer className="sync-footer">{info?.origin ?? location.origin}</footer>
      </main>
    </CakeProvider>
  )
}
