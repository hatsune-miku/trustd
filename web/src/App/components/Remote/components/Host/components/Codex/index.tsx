import { useEffect, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { Question } from './components/Question'

import type { Invoke } from '../..'
import { Feedback } from '../../../../../../../Feedback'
import { useAction } from '../../../../../../../useAction'

export type Pending = {
  id: string | number
  method: string
  params: {
    command?: string
    reason?: string
    permissions?: Record<string, unknown>
    message?: string
    url?: string
    requestedSchema?: unknown
    questions?: { id: string; question: string; options?: { label: string; description?: string }[] }[]
  }
}
type Agent = { id: string; cwd: string; status: string; last_message: string; error?: string; questions?: Pending[] }

export function Codex({
  invoke,
  sessionRef,
  target,
  cwd,
}: {
  invoke: Invoke
  sessionRef?: string
  target: string
  cwd: string
}) {
  const storageKey = `trustd.codex.${target}`
  const [id, setId] = useState(() => localStorage.getItem(storageKey) ?? '')
  const [agent, setAgent] = useState<Agent | null>(null)
  const [message, setMessage] = useState('')
  const [model, setModel] = useState('')
  const [history, setHistory] = useState<string[]>([])
  const [historyOffset, setHistoryOffset] = useState(0)
  const [historyEnd, setHistoryEnd] = useState(false)
  const [conversations, setConversations] = useState<Agent[]>([])
  const [conversationOffset, setConversationOffset] = useState<number | null>(0)
  const action = useAction()
  async function refresh() {
    if (id) setAgent(await invoke<Agent>('agent_get', { agent_id: id }))
  }
  useEffect(() => {
    if (!id) return
    let active = true
    async function poll() {
      try {
        const next = await invoke<Agent>('agent_get', { agent_id: id })
        if (active) setAgent(next)
      } catch {
        /* Keep the conversation visible while the host reconnects. */
      }
    }
    void poll()
    const timer = window.setInterval(() => void poll(), 2000)
    return () => {
      active = false
      window.clearInterval(timer)
    }
  }, [id])
  async function start() {
    const next = await invoke<Agent>('agent_start', {
      session_ref: sessionRef,
      ...(cwd ? { cwd } : {}),
      ...(model ? { model } : {}),
    })
    setAgent(next)
    setId(next.id)
    setHistory([])
    setHistoryOffset(0)
    setHistoryEnd(false)
    localStorage.setItem(storageKey, next.id)
  }
  async function send() {
    await invoke('agent_send', { agent_id: id, message })
    setHistory((value) => [...value, `我：${message}`])
    setMessage('')
    await refresh()
  }
  async function resume() {
    setAgent(await invoke<Agent>('agent_resume', { agent_id: id, session_ref: sessionRef, cwd: agent?.cwd || cwd }))
  }
  async function loadHistory() {
    const events = await invoke<{
      next_offset: number
      eof: boolean
      events: { method: string; params?: { item?: { type: string; text?: string; content?: { text?: string }[] } } }[]
    }>('agent_events', { agent_id: id, offset: historyOffset })
    setHistory((previous) => [
      ...previous,
      ...events.events
        .filter(
          (event) =>
            event.method === 'item/completed' &&
            ['agentMessage', 'userMessage'].includes(event.params?.item?.type ?? '')
        )
        .map((event) =>
          event.params?.item?.type === 'userMessage'
            ? `我：${event.params.item.content?.map((part) => part.text ?? '').join('')}`
            : (event.params?.item?.text ?? '')
        ),
    ])
    setHistoryOffset(events.next_offset)
    setHistoryEnd(events.eof)
  }
  async function listConversations() {
    const next = await invoke<{ agents: Agent[]; next_offset: number | null }>('agent_list', {
      offset: conversationOffset ?? 0,
    })
    setConversations((previous) => (conversationOffset ? [...previous, ...next.agents] : next.agents))
    setConversationOffset(next.next_offset)
  }
  function selectConversation(value: string) {
    setId(value)
    setAgent(null)
    setHistory([])
    setHistoryOffset(0)
    setHistoryEnd(false)
    localStorage.setItem(storageKey, value)
  }
  return (
    <div className="form-stack">
      <Feedback {...action} />
      <div className="remote-actions">
        <Button variant="ghost" disabled={action.busy} onClick={() => void action.run(listConversations)}>
          {conversationOffset ? '更多对话' : '已有对话'}
        </Button>
        {conversations.length > 0 && (
          <select
            className="remote-select"
            aria-label="选择对话"
            value={id}
            onChange={(event) => selectConversation(event.target.value)}
          >
            <option value="">选择对话</option>
            {conversations.map((conversation) => (
              <option key={conversation.id} value={conversation.id}>
                {conversation.id.slice(0, 8)} · {conversation.status} · {conversation.last_message.slice(0, 40)}
              </option>
            ))}
          </select>
        )}
      </div>
      <Field htmlFor="remote-codex-model" label="模型">
        <TextBox
          id="remote-codex-model"
          value={model}
          onChange={(event) => setModel(event.target.value)}
          placeholder="留空使用远端 Codex 设置"
        />
      </Field>
      <div className="remote-actions">
        <Button disabled={!sessionRef || action.busy} onClick={() => void action.run(start)}>
          新建对话
        </Button>
        {id && (
          <>
            <Button variant="ghost" disabled={!sessionRef || action.busy} onClick={() => void action.run(resume)}>
              恢复对话
            </Button>
            <Button
              variant="ghost"
              onClick={() =>
                void action.run(async () => {
                  await invoke('agent_interrupt', { agent_id: id })
                  await refresh()
                })
              }
            >
              中断当前轮次
            </Button>
            <Button
              variant="ghost"
              onClick={() =>
                void action.run(async () => {
                  await invoke('agent_close', { agent_id: id })
                  await refresh()
                })
              }
            >
              停止 Codex
            </Button>
          </>
        )}
      </div>
      {agent && (
        <>
          <div className="remote-actions">
            <strong>{agent.status}</strong>
            <Button variant="ghost" onClick={() => void action.run(loadHistory)}>
              {historyOffset && !historyEnd ? '继续读取记录' : '读取记录'}
            </Button>
            <Button variant="ghost" onClick={() => void action.run(refresh)}>
              刷新
            </Button>
          </div>
          {agent.error && <p>{agent.error}</p>}
          {agent.questions?.map((question) => (
            <Question
              key={String(question.id)}
              question={question}
              onAnswer={async (result) => {
                await invoke('agent_respond', { agent_id: id, request_id: question.id, result })
                await refresh()
              }}
            />
          ))}
          {history.length > 0 && <pre className="remote-output">{history.join('\n\n')}</pre>}
          {agent.last_message && <pre className="remote-output">{agent.last_message}</pre>}
          <form
            className="form-stack"
            onSubmit={(event) => {
              event.preventDefault()
              void action.run(send)
            }}
          >
            <label htmlFor="remote-codex-message">消息</label>
            <textarea
              className="remote-textarea"
              id="remote-codex-message"
              value={message}
              onChange={(event) => setMessage(event.target.value)}
              required
            />
            <Button
              type="submit"
              disabled={action.busy || ['running', 'waiting_user', 'starting', 'stopped'].includes(agent.status)}
            >
              发送
            </Button>
          </form>
        </>
      )}
    </div>
  )
}
