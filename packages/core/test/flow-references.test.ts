import { describe, expect, test } from 'bun:test'
import { FlowReferenceError, findFlowReferences } from '../src/flow-references.ts'

describe('findFlowReferences', () => {
  test('finds string and file forms for runFlow and runScript', () => {
    expect(findFlowReferences([
      '- runFlow: a.yaml',
      '- runFlow:',
      '    file: ../b.yaml',
      '    env: { X: 1 }',
      '- runScript: c.js',
      '- runScript:',
      '    file: d.js',
      '',
    ].join('\n'))).toEqual([
      { kind: 'flow', path: 'a.yaml' },
      { kind: 'flow', path: '../b.yaml' },
      { kind: 'script', path: 'c.js' },
      { kind: 'script', path: 'd.js' },
    ])
  })

  test('reads every document and hook, and looks inside repeat, retry and inline commands', () => {
    expect(findFlowReferences([
      'appId: app',
      'onFlowStart:',
      '  - runScript: start.js',
      'onFlowComplete:',
      '  - runFlow: done.yaml',
      '---',
      '- repeat:',
      '    times: 2',
      '    commands:',
      '      - retry:',
      '          commands:',
      '            - runFlow: deep.yaml',
      '- runFlow:',
      '    when:',
      '      visible: Ok',
      '    commands:',
      '      - runFlow: inline.yaml',
      '',
    ].join('\n')).map(reference => reference.path)).toEqual([
      'start.js', 'done.yaml', 'deep.yaml', 'inline.yaml',
    ])
  })

  test('finds quoted keys and flow-style spellings', () => {
    expect(findFlowReferences('- "runFlow": a.yaml\n- { runScript: { file: "b.js" } }\n- {"runFlow": {"file": "c.yaml"}}\n')
      .map(reference => reference.path)).toEqual(['a.yaml', 'b.js', 'c.yaml'])
  })

  test('finds nothing in a flow that runs no file, and rejects broken input', () => {
    expect(findFlowReferences('appId: app\n---\n- tapOn: Save\n')).toEqual([])
    expect(() => findFlowReferences('- runFlow:\n')).toThrow(FlowReferenceError)
    expect(() => findFlowReferences('- runScript:\n    env: {}\n')).toThrow(FlowReferenceError)
    expect(() => findFlowReferences('appId: app\n- assertVisible: Done\n')).toThrow(FlowReferenceError)
    expect(() => findFlowReferences('- runFlow: [unclosed')).toThrow(FlowReferenceError)
  })
})
