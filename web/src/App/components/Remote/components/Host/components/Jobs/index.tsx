import { useEffect, useRef, useState } from 'react'

import { Button } from '@a1knla/cakeui'

import type { Invoke } from '../..'
import { Feedback } from '../../../../../../../Feedback'
import { useAction } from '../../../../../../../useAction'

type Job = { id: string; command: string; status: string; result?: { exit_code: number | null }; error?: string }
type Chunk = { text: string; next_offset: number }

export function Jobs({
  invoke,
  sessionRef,
  execution,
}: {
  invoke: Invoke
  sessionRef?: string
  execution: Record<string, unknown>
}) {
  const [jobs, setJobs] = useState<Job[]>([])
  const [command, setCommand] = useState('')
  const [selected, setSelected] = useState('')
  const [stream, setStream] = useState('stdout')
  const [output, setOutput] = useState('')
  const cursor = useRef(0)
  const pending = useRef<{ key: string; request: string } | null>(null)
  const action = useAction()
  async function refresh() {
    setJobs((await invoke<{ jobs: Job[] }>('job_list')).jobs)
  }
  useEffect(() => {
    void action.run(refresh)
    const timer = window.setInterval(() => void refresh().catch(() => {}), 3000)
    return () => window.clearInterval(timer)
  }, [])
  useEffect(() => {
    cursor.current = 0
    setOutput('')
    if (!selected) return
    let active = true
    let loading = false
    async function read() {
      if (loading) return
      loading = true
      try {
        const chunk = await invoke<Chunk>('job_output', { job_id: selected, stream, offset: cursor.current })
        if (active) {
          cursor.current = chunk.next_offset
          setOutput((value) => (value + chunk.text).slice(-131072))
        }
      } catch {
        /* The task remains available after reconnect. */
      } finally {
        loading = false
      }
    }
    void read()
    const timer = window.setInterval(() => void read(), 1500)
    return () => {
      active = false
      window.clearInterval(timer)
    }
  }, [selected, stream])
  async function submit() {
    const args = { session_ref: sessionRef, command, ...execution }
    const request = JSON.stringify(args)
    if (!pending.current || pending.current.request !== request) pending.current = { key: crypto.randomUUID(), request }
    const job = await invoke<Job>('job_start', { ...args, idempotency_key: pending.current.key })
    pending.current = null
    setSelected(job.id)
    setCommand('')
    await refresh()
  }
  return (
    <div className="form-stack">
      <Feedback {...action} />
      <form
        className="form-stack"
        onSubmit={(event) => {
          event.preventDefault()
          void action.run(submit)
        }}
      >
        <label htmlFor="remote-command">命令</label>
        <textarea
          className="remote-textarea"
          id="remote-command"
          value={command}
          onChange={(event) => setCommand(event.target.value)}
          required
        />
        <Button type="submit" disabled={!sessionRef || action.busy}>
          提交任务
        </Button>
      </form>
      {jobs.map((job) => (
        <article className="remote-task" key={job.id}>
          <div className="remote-actions">
            <strong>{job.status}</strong>
            {job.result && <span>退出码 {job.result.exit_code ?? '—'}</span>}
            <Button variant="ghost" onClick={() => setSelected(job.id)}>
              查看输出
            </Button>
            {['starting', 'running'].includes(job.status) && (
              <Button
                variant="ghost"
                onClick={() =>
                  void action.run(async () => {
                    await invoke('job_cancel', { job_id: job.id })
                    await refresh()
                  })
                }
              >
                取消任务
              </Button>
            )}
          </div>
          <pre className="remote-task-command">{job.command}</pre>
          {job.error && <p>{job.error}</p>}
        </article>
      ))}
      {selected && (
        <div>
          <div className="remote-actions">
            {['stdout', 'stderr'].map((name) => (
              <Button key={name} variant={stream === name ? 'primary' : 'ghost'} onClick={() => setStream(name)}>
                {name}
              </Button>
            ))}
          </div>
          <pre className="remote-output">{output || '暂无输出'}</pre>
        </div>
      )}
    </div>
  )
}
