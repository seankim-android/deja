import { describe, expect, test } from 'bun:test'
import { parseProcStartTicks } from '../src/writer-lock.ts'

describe('Linux /proc/<pid>/stat parser', () => {
  // Fields after the name start at field 3 (state); the start time is field 22.
  const after = (start: string) => `S 1 1 1 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 ${start} 4096 100`
  test.each(['bash', 'a b', 'x) 9 (y', '((', 'a)b) c', ')', ''])('finds the start ticks with comm %j', comm => {
    expect(parseProcStartTicks(`42 (${comm}) ${after('987654')}\n`)).toBe('987654')
  })
  test('refuses a line without a name or without the field', () => {
    expect(parseProcStartTicks('garbage')).toBeNull()
    expect(parseProcStartTicks('42 (x) S 1 2')).toBeNull()
  })
})
