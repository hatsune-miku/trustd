import { useEffect, useState } from 'react'

import { Button } from '@a1knla/cakeui'

import { Feedback } from '../../../Feedback'
import { api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

type Kind = 'command' | 'audit'
type Entry = {
  timestamp: string
  event: string
  command?: string
  success?: boolean
  [key: string]: unknown
}
type Page = { entries: Entry[]; next_before: number | null }

export function Logs({ session }: { session: string }) {
  const [kind, setKind] = useState<Kind>('command')
  const [entries, setEntries] = useState<Entry[]>([])
  const [before, setBefore] = useState<number | null>(null)
  const action = useAction()

  async function load(older = false) {
    const query = new URLSearchParams({ kind })
    if (older && before !== null) query.set('before', String(before))
    const page = await api<Page>(`/v1/admin/logs?${query}`, undefined, session)
    setEntries((current) => (older ? [...current, ...page.entries] : page.entries))
    setBefore(page.next_before)
  }

  useEffect(() => {
    setEntries([])
    setBefore(null)
    void action.run(() => load())
  }, [session, kind])

  return (
    <section className="logs-view">
      <div className="logs-heading">
        <h2 className="section-title">日志</h2>
        <Button variant="ghost" disabled={action.busy} onClick={() => void action.run(() => load())}>
          刷新
        </Button>
      </div>
      <div className="logs-kinds" aria-label="日志类型">
        {(
          [
            ['command', '命令日志'],
            ['audit', '审计日志'],
          ] as const
        ).map(([value, label]) => (
          <Button
            key={value}
            variant={kind === value ? 'primary' : 'ghost'}
            aria-pressed={kind === value}
            disabled={action.busy}
            onClick={() => setKind(value)}
          >
            {label}
          </Button>
        ))}
      </div>
      <Feedback {...action} />
      {!entries.length && !action.busy && <p className="empty-state">暂无日志。</p>}
      <div className="logs-entries">
        {entries.map((entry, index) => (
          <article className="logs-entry" key={`${entry.timestamp}-${index}`}>
            <header className="logs-entry-heading">
              <time className="logs-entry-time" dateTime={entry.timestamp}>
                {new Date(entry.timestamp).toLocaleString('zh-CN', { hour12: false })}
              </time>
              <span className="logs-entry-event">{entry.event}</span>
              {entry.success !== undefined && (
                <span className="logs-entry-result">{entry.success ? '成功' : '失败'}</span>
              )}
            </header>
            {entry.command && <pre className="logs-command">{entry.command}</pre>}
            <details className="logs-details">
              <summary className="logs-details-summary">详情</summary>
              <pre className="logs-data">{JSON.stringify(entry, null, 2)}</pre>
            </details>
          </article>
        ))}
      </div>
      {before !== null && (
        <Button disabled={action.busy} onClick={() => void action.run(() => load(true))}>
          更早的日志
        </Button>
      )}
    </section>
  )
}
