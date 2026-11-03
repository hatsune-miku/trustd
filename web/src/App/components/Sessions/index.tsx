import { useEffect, useState } from 'react'

import { Button, Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@a1knla/cakeui'

import { Feedback } from '../../../Feedback'
import { type SessionRecord, api, dateTime } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

export function Sessions({ session }: { session: string }) {
  const [records, setRecords] = useState<SessionRecord[]>([])
  const action = useAction()
  async function refresh() {
    setRecords(await api<SessionRecord[]>('/v1/admin/sessions/list', {}, session))
  }
  useEffect(() => {
    void action.run(refresh)
  }, [session])
  async function revoke(id: string) {
    if (!window.confirm('终止此授权下的会话和执行中的命令？')) return false
    await api('/v1/admin/sessions/revoke', { id }, session)
    await refresh()
  }
  return (
    <section className="sessions-view">
      <div className="sessions-heading">
        <h2 className="section-title">已授权会话</h2>
        <Button variant="ghost" disabled={action.busy} onClick={() => void action.run(refresh)}>
          刷新
        </Button>
      </div>
      <Feedback {...action} />
      {!records.length ? (
        <p className="empty-state">暂无会话。</p>
      ) : (
        <div className="data-table-wrap">
          <Table className="data-table">
            <TableHead>
              <TableRow>
                <TableHeader>授权时间</TableHeader>
                <TableHeader>到期时间</TableHeader>
                <TableHeader>状态</TableHeader>
                <TableHeader>操作</TableHeader>
              </TableRow>
            </TableHead>
            <TableBody>
              {records.map((record) => {
                const expired = record.expires_at !== null && record.expires_at * 1000 <= Date.now()
                return (
                  <TableRow key={record.id}>
                    <TableCell>{dateTime(record.issued_at)}</TableCell>
                    <TableCell>{dateTime(record.expires_at)}</TableCell>
                    <TableCell>{record.revoked ? '已撤销' : expired ? '已到期' : '有效'}</TableCell>
                    <TableCell>
                      <Button
                        size="small"
                        variant="ghost"
                        disabled={record.revoked || expired || action.busy}
                        onClick={() => void action.run(() => revoke(record.id), '会话已撤销。')}
                      >
                        撤销
                      </Button>
                    </TableCell>
                  </TableRow>
                )
              })}
            </TableBody>
          </Table>
        </div>
      )}
    </section>
  )
}
