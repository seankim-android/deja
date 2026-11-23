import { describe, expect, test } from 'bun:test'
import { ASSERTION_HASH_VERSION, findFlowAssertions } from '../src/index.ts'

const entries = (yaml: string) => findFlowAssertions(yaml).map(assertion => assertion.entry)

// Golden vectors for assertion hash form version 1. A change to any line here changes every
// manifest's assertionsSha256, so it needs a new ASSERTION_HASH_VERSION and manifest hashVersion.
describe('flow assertions (hash form version 1)', () => {
  test('is version 1', () => {
    expect(ASSERTION_HASH_VERSION).toBe(1)
  })

  test('lists whole commands with sorted keys, whatever the YAML spelling', () => {
    const yaml = [
      'appId: com.example',
      '---',
      '- assertVisible:',
      '    id: "screen.today"',
      '- assertNotVisible:',
      '    text: "Error.*"',
      '    id: "today.error"',
      '- assertVisible: "Order confirmed"',
      '- assertTrue: ${output.ok}',
      '',
    ].join('\n')
    expect(entries(yaml)).toEqual([
      '|assertVisible|{"id":"screen.today"}',
      '|assertNotVisible|{"id":"today.error","text":"Error.*"}',
      '|assertVisible|"Order confirmed"',
      '|assertTrue|"${output.ok}"',
    ])
    expect(entries('- assertNotVisible: {text: "Error.*", id: "today.error"}\n')).toEqual([
      '|assertNotVisible|{"id":"today.error","text":"Error.*"}',
    ])
  })

  test('any extra field changes the entry', () => {
    const base = entries('- assertVisible:\n    id: "a"\n')
    expect(entries('- assertVisible:\n    id: "a"\n    optional: true\n')).not.toEqual(base)
    expect(entries('- assertVisible:\n    id: "a"\n    enabled: true\n')).not.toEqual(base)
  })

  test('records where an assertion sits, and the parameters of every runFlow, repeat and retry', () => {
    const yaml = [
      '- repeat:',
      '    times: 2',
      '    commands:',
      '      - assertVisible:',
      '          id: "row"',
      '- runFlow:',
      '    when:',
      '      visible: "Banner"',
      '    commands:',
      '      - assertVisible: "inline"',
      '- runFlow: ../subflows/type.yaml',
      '- retry:',
      '    maxRetries: 3',
      '    file: ../subflows/flaky.yaml',
      '',
    ].join('\n')
    expect(entries(yaml)).toEqual([
      '|repeat|params={"times":2}',
      'repeat{"times":2}>commands|assertVisible|{"id":"row"}',
      '|runFlow|params={"when":{"visible":"Banner"}}',
      'runFlow{"when":{"visible":"Banner"}}>commands|assertVisible|"inline"',
      '|runFlow|params={}',
      '|retry|params={"maxRetries":3}',
    ])
  })

  test('a weaker repeat, retry, condition or env value changes the entries', () => {
    const base = (times: string, extra = '') => entries(
      `- repeat:\n    times: ${times}\n    commands:\n      - assertVisible: "row"\n${extra}`)
    expect(base('2')).not.toEqual(base('0'))
    const looping = (condition: string) => entries(
      `- repeat:\n    while:\n      visible: "${condition}"\n    commands:\n      - assertVisible: "row"\n`)
    expect(looping('A')).not.toEqual(looping('B'))
    const flow = (env: string) => entries(`- runFlow:\n    file: a.yaml\n    env:\n      CODE: "${env}"\n`)
    expect(flow('1')).not.toEqual(flow('2'))
    expect(flow('1')).toEqual(['|runFlow|params={"env":{"CODE":"1"}}'])
    expect(entries('appId: a\nenv:\n  NAME: x\n---\n- assertVisible: "${NAME}"\n'))
      .toEqual(['|env|{"NAME":"x"}', '|assertVisible|"${NAME}"'])
    // A label is not behavior.
    expect(entries('- runFlow:\n    file: a.yaml\n    label: One\n')).toEqual(entries('- runFlow:\n    file: a.yaml\n    label: Two\n'))
  })

  test('counts inline scripts and reports which lines always run', () => {
    const yaml = [
      '- evalScript: ${output.ok = true}',
      '- assertTrue: ${output.ok}',
      '- assertVisible:',
      '    id: "soft"',
      '    optional: true',
      '- runFlow:',
      '    file: a.yaml',
      '    when:',
      '      platform: iOS',
      '- runFlow: b.yaml',
      '- runScript: c.js',
      '- repeat:',
      '    times: 1',
      '    commands:',
      '      - assertVisible: "inside"',
      '',
    ].join('\n')
    const items = findFlowAssertions(yaml)
    expect(items.map(item => [item.command, item.always])).toEqual([
      ['evalScript', true], ['assertTrue', true], ['assertVisible', false], ['runFlow', false],
      ['runFlow', true], ['runScript', true], ['repeat', true], ['assertVisible', false],
    ])
    expect(items[0].entry).toBe('|evalScript|"${output.ok = true}"')
    expect(items.filter(item => item.call).map(item => [item.call!.kind, item.call!.path]))
      .toEqual([['flow', 'a.yaml'], ['flow', 'b.yaml'], ['script', 'c.js']])
    expect(findFlowAssertions('appId: a\nonFlowStart:\n  - assertVisible: "x"\n').map(item => item.always)).toEqual([true])
  })

  test('a bare word assertion is an entry, and extendedWaitUntil is hashed with its whole mapping', () => {
    const yaml = '- assertNoDefectsWithAi\n- tapOn: x\n- extendedWaitUntil:\n    visible: "Done"\n    timeout: 5000\n'
    const items = findFlowAssertions(yaml)
    expect(items.map(item => item.entry)).toEqual([
      '|assertNoDefectsWithAi|null',
      '|extendedWaitUntil|{"timeout":5000,"visible":"Done"}',
    ])
    expect(items.map(item => item.command)).toEqual(['assertNoDefectsWithAi', 'extendedWaitUntil'])
    expect(entries('- extendedWaitUntil:\n    notVisible: "Done"\n    timeout: 5000\n'))
      .not.toEqual(entries('- extendedWaitUntil:\n    visible: "Done"\n    timeout: 5000\n'))
    expect(entries('- extendedWaitUntil:\n    visible: "Done"\n    timeout: 1\n'))
      .not.toEqual(entries('- extendedWaitUntil:\n    visible: "Done"\n    timeout: 5000\n'))
  })

  test('optional counts however YAML spells it, including through a merge key', () => {
    const always = (yaml: string) => findFlowAssertions(yaml).map(item => [item.command, item.always])
    expect(always('- assertVisible:\n    id: a\n    optional: true\n')).toEqual([['assertVisible', false]])
    expect(always('- assertVisible:\n    id: a\n    optional: yes\n')).toEqual([['assertVisible', false]])
    expect(always('- assertVisible:\n    id: a\n    optional: false\n')).toEqual([['assertVisible', true]])
    expect(always('- defaults: &base\n    optional: true\n- assertVisible:\n    <<: *base\n    id: a\n'))
      .toEqual([['assertVisible', false]])
    expect(always('- assertVisible:\n    <<: {optional: true}\n    id: a\n')).toEqual([['assertVisible', false]])
    expect(always('- runFlow:\n    file: a.yaml\n    optional: true\n')).toEqual([['runFlow', false]])
    expect(always('- runFlow:\n    file: a.yaml\n    optional: yes\n')).toEqual([['runFlow', false]])
    expect(always('- runFlow:\n    <<: {optional: true}\n    file: a.yaml\n')).toEqual([['runFlow', false]])
    expect(always('- runFlow:\n    optional: true\n    commands:\n      - assertVisible: a\n'))
      .toEqual([['runFlow', false], ['assertVisible', false]])
  })

  test('finds assertions in every document and in onFlowStart', () => {
    const yaml = 'appId: a\nonFlowStart:\n  - assertVisible: "start"\n---\n- assertVisible: "main"\n'
    expect(entries(yaml)).toEqual(['onFlowStart|assertVisible|"start"', '|assertVisible|"main"'])
  })

  test('repeats are kept and unreadable YAML throws', () => {
    expect(entries('- assertVisible: "a"\n- assertVisible: "a"\n')).toHaveLength(2)
    expect(() => findFlowAssertions('a: [unclosed')).toThrow()
  })

  test('exposes the id of an assertVisible', () => {
    expect(findFlowAssertions('- assertVisible:\n    id: "x"\n- assertVisible: "text"\n').map(a => a.id))
      .toEqual(['x', null])
  })
})
