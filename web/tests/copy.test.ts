import { describe, expect, it } from 'vitest'

import { dateTime } from '../src/api'

describe('session dates', () => {
  it('does not call a process-local session permanently persistent', () => {
    expect(dateTime(null)).toBe('直到撤销或服务重启')
  })
})
