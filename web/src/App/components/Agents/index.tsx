import { useEffect, useState } from 'react'

import { Button, ComboBox } from '@a1knla/cakeui'

import { Feedback } from '../../../Feedback'
import { type Agent, type AgentPolicy, api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

export function Agents({ session }: { session: string }) {
  const [agents, setAgents] = useState<Agent[]>([])
  const [loaded, setLoaded] = useState(false)
  const [scanError, setScanError] = useState('')
  const action = useAction()

  function receive(next: Agent[]) {
    setAgents((current) =>
      next.map((agent) => {
        const previous = current.find((item) => item.id === agent.id)
        return previous && previous.revision > agent.revision
          ? { ...agent, policy: previous.policy, revision: previous.revision }
          : agent
      })
    )
    setLoaded(true)
    setScanError('')
  }

  async function refresh() {
    receive((await api<{ agents: Agent[] }>('/v1/admin/agents', undefined, session)).agents)
  }

  useEffect(() => {
    const controller = new AbortController()
    async function scan() {
      try {
        const result = await api<{ agents: Agent[] }>('/v1/admin/agents', undefined, session, controller.signal)
        if (!controller.signal.aborted) receive(result.agents)
      } catch (error) {
        if (!controller.signal.aborted) setScanError(error instanceof Error ? error.message : String(error))
      }
    }
    void scan()
    const timer = window.setInterval(() => void scan(), 5000)
    return () => {
      controller.abort()
      window.clearInterval(timer)
    }
  }, [session])

  async function change(agent: Agent, policy: AgentPolicy) {
    await api(
      '/v1/admin/agents/policy',
      {
        id: agent.id,
        policy,
        revision: agent.revision,
        command_id: crypto.randomUUID(),
      },
      session
    )
    await refresh()
  }

  return (
    <section className="agents-view">
      <div className="agents-heading">
        <h2 className="section-title">Agent 授权</h2>
        <Button variant="ghost" disabled={action.busy} onClick={() => void action.run(refresh)}>
          刷新
        </Button>
      </div>
      <p className="description">按程序完整路径管理。同名但路径不同的程序分别授权。</p>
      <Feedback {...action} />
      {scanError && (
        <p className="agents-error" role="alert">
          扫描失败：{scanError}
        </p>
      )}
      {!agents.length && (
        <p className="empty-state">{loaded ? '尚未发现 Agent 进程。启动 Agent 后会自动显示。' : '正在扫描本机进程…'}</p>
      )}
      <div className="agents-list">
        {agents.map((agent) => (
          <article className="agent-row" key={agent.id}>
            <div className="agent-identity">
              <h3 className="agent-name">{agent.name}</h3>
              <p className="agent-path">{agent.path ?? '无法读取程序路径'}</p>
              <p className="agent-processes">
                {agent.processes.length ? `运行中 · PID ${agent.processes.map((p) => p.pid).join('、')}` : '未运行'}
              </p>
            </div>
            <div className="agent-policy">
              <ComboBox
                aria-label={`${agent.name} ${agent.path ?? agent.id} 的授权状态`}
                disabled={action.busy || !agent.path}
                value={agent.policy}
                onChange={(event) => void action.run(() => change(agent, event.currentTarget.value as AgentPolicy))}
              >
                <option value="once">授权 1 次</option>
                <option value="trust">长期信任</option>
                <option value="ask">无预授权</option>
                <option value="deny">总是拒绝</option>
              </ComboBox>
              <span className="agent-policy-note">
                {
                  {
                    once: '下一次请求后恢复询问',
                    trust: '此路径不限次数自动允许',
                    ask: '每次请求都询问',
                    deny: '此路径的请求均拒绝',
                  }[agent.policy]
                }
              </span>
            </div>
          </article>
        ))}
      </div>
    </section>
  )
}
