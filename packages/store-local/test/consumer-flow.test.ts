import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { assertionsSha256, closureAssertions, collectFlowClosure } from '../src/index.ts'

let root = ''

beforeEach(async () => {
  root = await realpath(await mkdtemp(join(tmpdir(), 'deja-consumer-flow-')))
  await mkdir(join(root, '.maestro/flows'), { recursive: true })
  await mkdir(join(root, '.maestro/subflows'), { recursive: true })
})

afterEach(async () => {
  await rm(root, { recursive: true, force: true })
})

async function closureOf(files: Record<string, string>, main: string) {
  for (const [path, contents] of Object.entries(files)) await writeFile(join(root, path), contents)
  return collectFlowClosure(root, join(root, main))
}

const MAIN = 'appId: com.example\n---\n- assertVisible:\n    id: "screen.today"\n- runFlow: ../subflows/sub.yaml\n- runScript: ../subflows/check.js\n'
const SUB = '- assertNotVisible:\n    id: "today.error"\n'
const SCRIPT = 'output.ok = true\n'
const files = {
  '.maestro/flows/main.yaml': MAIN,
  '.maestro/subflows/sub.yaml': SUB,
  '.maestro/subflows/check.js': SCRIPT,
}

describe('closure assertions (hash form version 1)', () => {
  test('lists assertions, call conditions and script hashes across the closure', async () => {
    const closure = await closureOf(files, '.maestro/flows/main.yaml')
    expect(closure.files.map(file => file.kind)).toEqual(['flow', 'script', 'flow'])
    expect(closureAssertions(closure).map(assertion => assertion.entry).sort()).toEqual([
      '|assertNotVisible|{"id":"today.error"}',
      '|assertVisible|{"id":"screen.today"}',
      '|runFlow|params={}',
      '|runScript|sha256=1bd1958618755ba737b2107d401518d9efac80e6baaf85b3a46c54ac44a6c6a9',
    ].sort())
  })

  test('hash golden vector', async () => {
    const closure = await closureOf(files, '.maestro/flows/main.yaml')
    expect(closure.sha256).toBe('0390f482a200fb8e79b697e5d9cd8afaf313ed0c911aceb651762c1746e254d3')
    expect(assertionsSha256(closure)).toBe('2bc5bbb3f2fd1d009dac2ab61f905cf72638efc31ef9d97440cc7912b46087a3')
  })

  test('is stable under reordering and file renames, and moves with a changed assertion or script', async () => {
    const base = assertionsSha256(await closureOf(files, '.maestro/flows/main.yaml'))
    // Selector and text edits that are not assertions leave it alone.
    await writeFile(join(root, '.maestro/flows/main.yaml'), MAIN.replace('appId: com.example', 'appId: com.example\nname: Renamed'))
    expect(assertionsSha256(await collectFlowClosure(root, join(root, '.maestro/flows/main.yaml')))).toBe(base)
    // Moving an assertion into the subflow keeps the hash (it still runs, unconditionally).
    await writeFile(join(root, '.maestro/flows/main.yaml'), MAIN.replace('- assertVisible:\n    id: "screen.today"\n', ''))
    await writeFile(join(root, '.maestro/subflows/sub.yaml'), `${SUB}- assertVisible:\n    id: "screen.today"\n`)
    expect(assertionsSha256(await collectFlowClosure(root, join(root, '.maestro/flows/main.yaml')))).toBe(base)
    // Dropping one, or touching the script, does not.
    await writeFile(join(root, '.maestro/subflows/sub.yaml'), SUB)
    expect(assertionsSha256(await collectFlowClosure(root, join(root, '.maestro/flows/main.yaml')))).not.toBe(base)
    await writeFile(join(root, '.maestro/flows/main.yaml'), MAIN)
    await writeFile(join(root, '.maestro/subflows/check.js'), 'output.ok = false\n')
    expect(assertionsSha256(await collectFlowClosure(root, join(root, '.maestro/flows/main.yaml')))).not.toBe(base)
  })
})
