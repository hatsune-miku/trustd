export type Health = { daemon: 'user' | 'privileged'; version: string; commit: string }
export type LoginResult = { session: string; expires_at: number }
export type Settings = {
  daemon: 'user' | 'privileged'
  totp_enabled: boolean
  passkeys: { id: string; name: string; created_at: number }[]
  paired_peer: string | null
  desktop_available: boolean
  passkey_origin: string
}
export type SessionRecord = { id: string; issued_at: number; expires_at: number | null; revoked: boolean }
export type AgentPolicy = 'once' | 'trust' | 'ask' | 'deny'
export type Agent = {
  id: string
  name: string
  path: string | null
  processes: { pid: number; started_at: number }[]
  policy: AgentPolicy
  revision: number
}

export class ApiError extends Error {
  constructor(
    message: string,
    public readonly status: number
  ) {
    super(message)
  }
}

export async function api<T>(path: string, body?: unknown, session?: string, signal?: AbortSignal): Promise<T> {
  const headers: Record<string, string> = {}
  if (body !== undefined) headers['Content-Type'] = 'application/json'
  if (session) headers.Authorization = `Bearer ${session}`
  const response = await fetch(path, {
    method: body === undefined ? 'GET' : 'POST',
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: 'omit',
    cache: 'no-store',
    signal,
  })
  const data = await response.json().catch(() => ({ error: `HTTP ${response.status}: ${response.statusText}` }))
  if (!response.ok) throw new ApiError(data.error || `HTTP ${response.status}`, response.status)
  return data as T
}

export function dateTime(value: number | null) {
  return value === null ? '直到撤销或服务重启' : new Date(value * 1000).toLocaleString('zh-CN', { hour12: false })
}
