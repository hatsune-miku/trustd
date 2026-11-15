import { useState } from 'react'

import { Button } from '@a1knla/cakeui'

import type { Pending } from '../..'
import { Feedback } from '../../../../../../../../../Feedback'
import { useAction } from '../../../../../../../../../useAction'

export function Question({ question, onAnswer }: { question: Pending; onAnswer: (value: unknown) => Promise<void> }) {
  const [answers, setAnswers] = useState<Record<string, string>>({})
  const action = useAction()
  const [formValue, setFormValue] = useState('{}')
  async function approve(accept: boolean) {
    if (question.method === 'item/permissions/requestApproval') {
      await onAnswer({ permissions: accept ? question.params.permissions : {}, scope: 'turn' })
    } else if (question.method === 'mcpServer/elicitation/request') {
      await onAnswer({
        action: accept ? 'accept' : 'decline',
        ...(accept && question.params.requestedSchema ? { content: JSON.parse(formValue) } : {}),
      })
    } else {
      await onAnswer({ decision: accept ? 'accept' : 'decline' })
    }
  }
  return (
    <div className="remote-question">
      <Feedback {...action} />
      {question.params.message && <p>{question.params.message}</p>}
      {question.params.url && /^https?:\/\//i.test(question.params.url) && (
        <a className="content-link" href={question.params.url} target="_blank" rel="noreferrer">
          打开请求链接
        </a>
      )}
      {Boolean(question.params.requestedSchema) && (
        <label className="form-stack">
          回答内容（JSON）
          <pre className="remote-output">{JSON.stringify(question.params.requestedSchema, null, 2)}</pre>
          <textarea
            className="remote-textarea"
            value={formValue}
            onChange={(event) => setFormValue(event.target.value)}
          />
        </label>
      )}
      {question.params.reason && <p>{question.params.reason}</p>}
      {question.params.command && <pre className="remote-task-command">{question.params.command}</pre>}
      {question.params.permissions && (
        <pre className="remote-task-command">{JSON.stringify(question.params.permissions, null, 2)}</pre>
      )}
      {question.params.questions ? (
        <form
          className="form-stack"
          onSubmit={(event) => {
            event.preventDefault()
            void action.run(() =>
              onAnswer({
                answers: Object.fromEntries(Object.entries(answers).map(([id, value]) => [id, { answers: [value] }])),
              })
            )
          }}
        >
          {question.params.questions.map((item) => (
            <label className="form-stack" key={item.id}>
              {item.question}
              {item.options && (
                <select
                  className="remote-select"
                  value={answers[item.id] ?? ''}
                  onChange={(event) => setAnswers((value) => ({ ...value, [item.id]: event.target.value }))}
                >
                  <option value="">请选择</option>
                  {item.options.map((option) => (
                    <option key={option.label} value={option.label}>
                      {option.label}
                    </option>
                  ))}
                </select>
              )}
              <textarea
                className="remote-textarea"
                value={answers[item.id] ?? ''}
                onChange={(event) => setAnswers((value) => ({ ...value, [item.id]: event.target.value }))}
                required
              />
            </label>
          ))}
          <Button type="submit" disabled={action.busy}>
            提交回答
          </Button>
        </form>
      ) : (
        <div className="remote-actions">
          <Button disabled={action.busy} onClick={() => void action.run(() => approve(true))}>
            允许
          </Button>
          <Button variant="ghost" disabled={action.busy} onClick={() => void action.run(() => approve(false))}>
            拒绝
          </Button>
        </div>
      )}
    </div>
  )
}
