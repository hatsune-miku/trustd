import { useRef, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import type { Invoke } from '../..'
import { Feedback } from '../../../../../../../Feedback'
import { useAction } from '../../../../../../../useAction'

type Chunk = { data: string; next_offset: number; eof: boolean }

export function Files({ invoke, sessionRef }: { invoke: Invoke; sessionRef?: string }) {
  const [path, setPath] = useState('')
  const [file, setFile] = useState<File | null>(null)
  const [replace, setReplace] = useState(false)
  const [progress, setProgress] = useState('')
  const [result, setResult] = useState('')
  const action = useAction()
  const transferState = useRef<{ file: File; path: string; replace: boolean; id: string; offset: number } | null>(null)
  async function inspect() {
    setResult(JSON.stringify(await invoke('file_stat', { session_ref: sessionRef, path, checksum: true }), null, 2))
  }
  async function upload() {
    if (!file) return
    if (
      transferState.current?.file !== file ||
      transferState.current.path !== path ||
      transferState.current.replace !== replace
    )
      transferState.current = { file, path, replace, id: crypto.randomUUID(), offset: 0 }
    const transfer = transferState.current
    for (let offset = transfer.offset; offset < file.size || (file.size === 0 && offset === 0); offset += 32768) {
      const chunk = new Uint8Array(await file.slice(offset, offset + 32768).arrayBuffer())
      const data = btoa(String.fromCharCode(...chunk))
      const completed = offset + chunk.length >= file.size
      const response = await invoke('file_write', {
        session_ref: sessionRef,
        path,
        transfer_id: transfer.id,
        offset,
        data,
        complete: completed,
        replace,
      })
      setProgress(`${Math.min(offset + chunk.length, file.size)} / ${file.size} bytes`)
      if (completed) {
        setResult(JSON.stringify(response, null, 2))
        transferState.current = null
        break
      }
      transfer.offset = offset + chunk.length
    }
  }
  async function download() {
    type SaveWindow = Window & {
      showSaveFilePicker?: (options: { suggestedName: string }) => Promise<{
        createWritable: () => Promise<{
          write: (bytes: Uint8Array) => Promise<void>
          close: () => Promise<void>
          abort: () => Promise<void>
        }>
      }>
    }
    const picker = (window as SaveWindow).showSaveFilePicker
    const filename = path.split(/[\\/]/).pop() || 'download'
    const writable = picker ? await (await picker.call(window, { suggestedName: filename })).createWritable() : null
    const chunks: BlobPart[] = []
    let offset = 0
    try {
      const metadata = await invoke<{ size: number }>('file_stat', { session_ref: sessionRef, path })
      if (!picker && metadata.size > 64 * 1024 * 1024)
        throw new Error('此浏览器不支持大文件流式保存，请使用 Chrome 或 Edge。')
      for (;;) {
        const chunk = await invoke<Chunk>('file_read', {
          session_ref: sessionRef,
          path,
          offset,
          limit: 1024 * 1024,
          encoding: 'base64',
        })
        const bytes = Uint8Array.from(atob(chunk.data), (value) => value.charCodeAt(0))
        if (writable) await writable.write(bytes)
        else chunks.push(bytes)
        offset = chunk.next_offset
        setProgress(`${offset} / ${metadata.size} bytes`)
        if (chunk.eof) break
      }
      if (writable) await writable.close()
      else {
        const url = URL.createObjectURL(new Blob(chunks))
        const anchor = document.createElement('a')
        anchor.href = url
        anchor.download = filename
        anchor.click()
        window.setTimeout(() => URL.revokeObjectURL(url), 1000)
      }
    } catch (error) {
      if (writable) await writable.abort()
      throw error
    }
  }
  return (
    <div className="form-stack">
      <Feedback {...action} />
      <p>以远端当前用户的权限访问文件。</p>
      <Field label="远端绝对路径" htmlFor="remote-file-path">
        <TextBox id="remote-file-path" value={path} onChange={(event) => setPath(event.target.value)} />
      </Field>
      <div className="remote-actions">
        <Button disabled={!sessionRef || !path || action.busy} onClick={() => void action.run(inspect)}>
          查看信息
        </Button>
        <Button disabled={!sessionRef || !path || action.busy} onClick={() => void action.run(download)}>
          下载
        </Button>
      </div>
      <input
        className="remote-file-input"
        type="file"
        aria-label="上传文件"
        onChange={(event) => setFile(event.target.files?.[0] ?? null)}
      />
      <label className="remote-check">
        <input
          className="remote-checkbox"
          type="checkbox"
          checked={replace}
          onChange={(event) => setReplace(event.target.checked)}
        />
        替换已有文件
      </label>
      <Button disabled={!file || !sessionRef || !path || action.busy} onClick={() => void action.run(upload)}>
        上传
      </Button>
      {progress && <p>{progress}</p>}
      {result && <pre className="remote-output">{result}</pre>}
    </div>
  )
}
