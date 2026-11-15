import { useEffect, useState } from 'react'

import { Button } from '@a1knla/cakeui'

import type { Invoke } from '../..'
import { Feedback } from '../../../../../../../Feedback'
import { useAction } from '../../../../../../../useAction'

type Page = { entries: Record<string, unknown>[]; next_before: number | null }
export function RemoteLogs({ invoke }: { invoke: Invoke }) {
  const [kind, setKind] = useState('command')
  const [page, setPage] = useState<Page>({ entries: [], next_before: null })
  const action = useAction()
  async function load(older = false) {
    const next = await invoke<Page>('logs_page', {
      kind,
      ...(older && page.next_before !== null ? { before: page.next_before } : {}),
    })
    setPage((value) => ({ ...next, entries: older ? [...value.entries, ...next.entries] : next.entries }))
  }
  useEffect(() => {
    void action.run(() => load())
  }, [kind])
  return (
    <div className="form-stack">
      <div className="remote-actions">
        {[
          ['command', '命令日志'],
          ['audit', '审计日志'],
        ].map(([id, label]) => (
          <Button key={id} variant={kind === id ? 'primary' : 'ghost'} onClick={() => setKind(id)}>
            {label}
          </Button>
        ))}
        <Button variant="ghost" onClick={() => void action.run(() => load())}>
          刷新
        </Button>
      </div>
      <Feedback {...action} />
      {page.entries.map((entry, index) => (
        <pre className="remote-output" key={index}>
          {JSON.stringify(entry, null, 2)}
        </pre>
      ))}
      {page.next_before !== null && <Button onClick={() => void action.run(() => load(true))}>更早的日志</Button>}
    </div>
  )
}
