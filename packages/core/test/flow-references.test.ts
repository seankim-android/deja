import { describe, expect, test } from 'bun:test'
import { FlowFileCommandError, FlowReferenceError, findFlowReferences } from '../src/flow-references.ts'

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
      { kind: 'flow', command: 'runFlow', path: 'a.yaml' },
      { kind: 'flow', command: 'runFlow', path: '../b.yaml' },
      { kind: 'script', command: 'runScript', path: 'c.js' },
      { kind: 'script', command: 'runScript', path: 'd.js' },
    ])
  })

  test('finds addMedia files in each spelling, and a retry that names a file', () => {
    expect(findFlowReferences([
      '- addMedia:',
      '    - one.png',
      '    - two.png',
      '    - ../clips/three.mp4',
      '- addMedia:',
      '    files: [four.jpg]',
      '- retry:',
      '    maxRetries: 2',
      '    file: again.yaml',
      '- retry:',
      '    commands:',
      '      - addMedia: [nested.gif]',
      '',
    ].join('\n'))).toEqual([
      { kind: 'media', command: 'addMedia', path: 'one.png' },
      { kind: 'media', command: 'addMedia', path: 'two.png' },
      { kind: 'media', command: 'addMedia', path: '../clips/three.mp4' },
      { kind: 'media', command: 'addMedia', path: 'four.jpg' },
      { kind: 'flow', command: 'retry', path: 'again.yaml' },
      { kind: 'media', command: 'addMedia', path: 'nested.gif' },
    ])
  })

  test('refuses addMedia without paths, and a command that reads a file it cannot follow', () => {
    expect(() => findFlowReferences('- addMedia:\n')).toThrow(FlowReferenceError)
    expect(() => findFlowReferences('- addMedia: []\n')).toThrow(FlowReferenceError)
    expect(() => findFlowReferences('- addMedia:\n    - 3\n')).toThrow(FlowReferenceError)
    // Maestro 2.12 takes a list or a `files` list, not a bare path.
    expect(() => findFlowReferences('- addMedia: one.png\n')).toThrow(FlowFileCommandError)
    expect(() => findFlowReferences('- addMedia:\n    file: one.png\n')).toThrow(FlowFileCommandError)
    expect(() => findFlowReferences('- retry: again.yaml\n')).toThrow(FlowFileCommandError)
    expect(() => findFlowReferences('- assertScreenshot: home.png\n')).toThrow(/reference image/)
    expect(() => findFlowReferences('- repeat:\n    commands:\n      - assertScreenshot: home\n')).toThrow(FlowReferenceError)
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
