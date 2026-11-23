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

async function hashOf(files: Record<string, string>, main = '.maestro/flows/main.yaml') {
  for (const [path, contents] of Object.entries(files)) await writeFile(join(root, path), contents)
  return assertionsSha256(await collectFlowClosure(root, join(root, main)))
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
  test('every line carries its file, its call chain and its place, in running order', async () => {
    await hashOf(files)
    const closure = await collectFlowClosure(root, join(root, '.maestro/flows/main.yaml'))
    expect(closure.files.map(file => file.kind)).toEqual(['flow', 'script', 'flow'])
    expect(closureAssertions(closure).map(assertion => assertion.entry)).toEqual([
      'flows/main.yaml|||assertVisible|{"id":"screen.today"}',
      'flows/main.yaml|||runFlow|params={}',
      'subflows/sub.yaml|flows/main.yaml|||runFlow|params={}||assertNotVisible|{"id":"today.error"}',
      'flows/main.yaml|||runScript|params={}',
      'flows/main.yaml|||runScript|params={}|script=subflows/check.js|sha256=1bd1958618755ba737b2107d401518d9efac80e6baaf85b3a46c54ac44a6c6a9',
    ])
  })

  test('hash golden vector', async () => {
    await hashOf(files)
    const closure = await collectFlowClosure(root, join(root, '.maestro/flows/main.yaml'))
    expect(closure.sha256).toBe('0390f482a200fb8e79b697e5d9cd8afaf313ed0c911aceb651762c1746e254d3')
    expect(assertionsSha256(closure)).toBe('67aa68ae3df829493086e2b37a13c8663d4634976c7f545c91493dea7b64d2aa')
  })

  test('is stable under edits that are not assertions', async () => {
    const base = await hashOf(files)
    expect(await hashOf({ ...files, '.maestro/flows/main.yaml': MAIN.replace('appId: com.example', 'appId: com.example\nname: Renamed') })).toBe(base)
    expect(await hashOf({ ...files, '.maestro/flows/main.yaml': MAIN.replace('- runFlow: ../subflows/sub.yaml', '- tapOn: x\n- runFlow: ../subflows/sub.yaml') })).toBe(base)
  })

  test('weakening changes it: a dropped or edited assertion, a script, an unconditional move between files', async () => {
    const base = await hashOf(files)
    expect(await hashOf({ ...files, '.maestro/subflows/sub.yaml': '- tapOn: x\n' })).not.toBe(base)
    expect(await hashOf({ ...files, '.maestro/subflows/sub.yaml': SUB, '.maestro/subflows/check.js': 'output.ok = false\n' })).not.toBe(base)
    // The assertion moves into the subflow, with nothing conditional around it: the file is part of the line.
    const moved = await hashOf({
      ...files,
      '.maestro/flows/main.yaml': MAIN.replace('- assertVisible:\n    id: "screen.today"\n', ''),
      '.maestro/subflows/sub.yaml': `${SUB}- assertVisible:\n    id: "screen.today"\n`,
      '.maestro/subflows/check.js': SCRIPT,
    })
    expect(moved).not.toBe(base)
  })

  test('review repro 1: an assertion moved into a subflow that is called under a condition', async () => {
    const strong = await hashOf({
      '.maestro/flows/main.yaml': 'appId: a\n---\n- assertVisible: "Saved"\n- runFlow:\n    file: ../subflows/sub.yaml\n    when:\n      platform: iOS\n',
      '.maestro/subflows/sub.yaml': '- tapOn: "x"\n',
    })
    const weak = await hashOf({
      '.maestro/flows/main.yaml': 'appId: a\n---\n- runFlow:\n    file: ../subflows/sub.yaml\n    when:\n      platform: iOS\n',
      '.maestro/subflows/sub.yaml': '- tapOn: "x"\n- assertVisible: "Saved"\n',
    })
    expect(weak).not.toBe(strong)
    // The call's condition is part of every line below it, so a changed condition changes the hash too.
    const loosened = await hashOf({
      '.maestro/flows/main.yaml': 'appId: a\n---\n- runFlow:\n    file: ../subflows/sub.yaml\n    when:\n      platform: Android\n',
      '.maestro/subflows/sub.yaml': '- tapOn: "x"\n- assertVisible: "Saved"\n',
    })
    expect(loosened).not.toBe(weak)
  })

  test('round 2: a bare assertion is hashed and counted, so deleting it changes both', async () => {
    const withBare = 'appId: a\n---\n- assertVisible: "One"\n- assertNoDefectsWithAi\n'
    const without = 'appId: a\n---\n- assertVisible: "One"\n'
    const count = async (text: string) => {
      await writeFile(join(root, '.maestro/flows/main.yaml'), text)
      const closure = await collectFlowClosure(root, join(root, '.maestro/flows/main.yaml'))
      return { count: closureAssertions(closure).filter(item => item.command.startsWith('assert')).length, hash: assertionsSha256(closure) }
    }
    const before = await count(withBare)
    const after = await count(without)
    expect(before.count).toBe(2)
    expect(after.count).toBe(1)
    expect(after.hash).not.toBe(before.hash)
  })

  test('round 2: extendedWaitUntil is part of the hash', async () => {
    const wait = (extra: string) => `appId: a\n---\n- extendedWaitUntil:\n    visible: "Done"\n${extra}`
    const base = await hashOf({ '.maestro/flows/main.yaml': wait('    timeout: 5000\n') })
    expect(await hashOf({ '.maestro/flows/main.yaml': wait('    timeout: 1\n') })).not.toBe(base)
    expect(await hashOf({ '.maestro/flows/main.yaml': 'appId: a\n---\n- tapOn: x\n' })).not.toBe(base)
  })

  test('round 2: an optional runFlow, in any spelling, takes the assertions below it off the owner floor', async () => {
    const reached = async (call: string) => {
      await writeFile(join(root, '.maestro/subflows/sub.yaml'), '- assertVisible:\n    id: saved\n')
      await writeFile(join(root, '.maestro/flows/main.yaml'), `appId: a\n---\n${call}`)
      const closure = await collectFlowClosure(root, join(root, '.maestro/flows/main.yaml'))
      return closureAssertions(closure).find(item => item.command === 'assertVisible')!.reachedAlways
    }
    expect(await reached('- runFlow: ../subflows/sub.yaml\n')).toBe(true)
    expect(await reached('- runFlow:\n    file: ../subflows/sub.yaml\n    optional: false\n')).toBe(true)
    expect(await reached('- runFlow:\n    file: ../subflows/sub.yaml\n    optional: true\n')).toBe(false)
    expect(await reached('- runFlow:\n    file: ../subflows/sub.yaml\n    optional: yes\n')).toBe(false)
    expect(await reached('- defaults: &opt\n    optional: true\n- runFlow:\n    <<: *opt\n    file: ../subflows/sub.yaml\n')).toBe(false)
    expect(await reached('- runFlow:\n    <<: {optional: true}\n    file: ../subflows/sub.yaml\n')).toBe(false)
  })

  test('review repro 2: an inline evalScript that feeds assertTrue', async () => {
    const flow = (source: string) => `appId: a\n---\n- evalScript: \${${source}}\n- assertTrue: \${output.ok}\n`
    expect(await hashOf({ '.maestro/flows/main.yaml': flow('output.ok = true') }))
      .not.toBe(await hashOf({ '.maestro/flows/main.yaml': flow('output.ok = false') }))
  })

  test('order counts: the assertions and calls keep the order the flow runs them in', async () => {
    const check = (order: string[]) => `appId: a\n---\n${order.join('\n')}\n`
    expect(await hashOf({ '.maestro/flows/main.yaml': check(['- assertVisible: "One"', '- assertVisible: "Two"']) }))
      .not.toBe(await hashOf({ '.maestro/flows/main.yaml': check(['- assertVisible: "Two"', '- assertVisible: "One"']) }))
    // An assertion moved before the action it checks is not seen: actions are not in the hash.
    expect(await hashOf({ '.maestro/flows/main.yaml': check(['- tapOn: save', '- assertVisible: "Saved"']) }))
      .toBe(await hashOf({ '.maestro/flows/main.yaml': check(['- assertVisible: "Saved"', '- tapOn: save']) }))
  })

  test('a repeat that runs zero times, and a value passed in env, change it', async () => {
    const looped = (times: number) => `appId: a\n---\n- repeat:\n    times: ${times}\n    commands:\n      - assertVisible: "row"\n`
    expect(await hashOf({ '.maestro/flows/main.yaml': looped(2) })).not.toBe(await hashOf({ '.maestro/flows/main.yaml': looped(0) }))
    const withEnv = (value: string) => `appId: a\n---\n- runFlow:\n    file: ../subflows/sub.yaml\n    env:\n      CODE: "${value}"\n`
    const sub = { '.maestro/subflows/sub.yaml': '- assertVisible: ${CODE}\n' }
    expect(await hashOf({ ...sub, '.maestro/flows/main.yaml': withEnv('1') }))
      .not.toBe(await hashOf({ ...sub, '.maestro/flows/main.yaml': withEnv('2') }))
  })
})
