import { describe, expect, test } from 'bun:test'
import {
  VERIFIED_SUBFLOW_SCHEMA_VERSION,
  InvalidSubflowError,
  sameSubflowIdentity,
  subflowMissReason,
  validateSubflowContract,
  type SubflowContract,
  type VerifiedSubflow,
} from '../src/subflow.ts'

function contract(): SubflowContract {
  return {
    schemaVersion: VERIFIED_SUBFLOW_SCHEMA_VERSION,
    segmentId: 'login-v1',
    identity: {
      appId: 'fixture.app', platform: 'fixture-platform', flowSchemaVersion: 'flow/v1',
      buildFingerprint: 'a'.repeat(64),
    },
    inputs: [
      { name: 'username', kind: 'text', required: true },
      { name: 'password', kind: 'credential', required: true },
    ],
    outputs: [{ name: 'catalog', kind: 'screen', resourceId: 'screen_catalog' }],
    preconditions: [{ kind: 'clean-app-launch' }],
    postconditions: [{ kind: 'screen-visible', resourceId: 'screen_catalog' }],
    stepCount: 11,
    assertionCount: 5,
    provenance: {
      kind: 'deterministic-fixture', fixtureId: 'login-contract-fixture', newDeviceRunRecorded: false,
    },
  }
}

function changed(mutate: (value: Record<string, unknown>) => void): unknown {
  const value = structuredClone(contract()) as unknown as Record<string, unknown>
  mutate(value)
  return value
}

describe('typed reusable subflow contracts', () => {
  test('round-trips closed typed inputs, outputs, identity, and conditions', () => {
    const value = validateSubflowContract(contract())
    expect(value).toEqual(contract())
    expect(value.inputs.map(input => input.kind)).toEqual(['text', 'credential'])
    expect(value.outputs[0]).toEqual({
      name: 'catalog', kind: 'screen', resourceId: 'screen_catalog',
    })
    expect(value.preconditions).toEqual([{ kind: 'clean-app-launch' }])
    expect(value.postconditions).toEqual([{ kind: 'screen-visible', resourceId: 'screen_catalog' }])
  })

  test('retains optional adapter and browser as exact app identity', () => {
    const value = contract()
    value.identity.adapter = 'fixture-adapter'
    value.identity.browser = 'fixture-browser'
    expect(validateSubflowContract(value).identity).toEqual(value.identity)
  })

  test('rejects unsupported versions and unknown fields', () => {
    expect(() => validateSubflowContract(changed(value => { value.schemaVersion = 2 })))
      .toThrow(InvalidSubflowError)
    expect(() => validateSubflowContract(changed(value => { value.unrestricted = {} })))
      .toThrow('missing or unsupported fields')
    expect(() => validateSubflowContract(changed(value => {
      value.identity = { ...value.identity as object, goal: 'not segment identity' }
    }))).toThrow('identity has unsupported fields')
  })

  test('rejects raw input values instead of persisting them in metadata', () => {
    expect(() => validateSubflowContract(changed(value => {
      value.inputs = [{ name: 'password', kind: 'credential', required: true, value: 'secret' }]
    }))).toThrow('inputs[0] has missing or unsupported fields')
  })

  test('rejects duplicate or malformed typed inputs and outputs', () => {
    expect(() => validateSubflowContract(changed(value => {
      value.inputs = [
        { name: 'same', kind: 'text', required: true },
        { name: 'same', kind: 'credential', required: true },
      ]
    }))).toThrow('input names must be unique')
    expect(() => validateSubflowContract(changed(value => { value.outputs = [] })))
      .toThrow('outputs must contain 1 to 8 entries')
    expect(() => validateSubflowContract(changed(value => {
      value.outputs = [{ name: 'raw', kind: 'screenshot', resourceId: 'anything' }]
    }))).toThrow('kind must be screen')
  })

  test('requires executable bounds and stable resource identifiers', () => {
    expect(validateSubflowContract(contract()).stepCount).toBe(11)
    const malformed = contract()
    malformed.stepCount = 0
    expect(() => validateSubflowContract(malformed)).toThrow('stepCount')
    malformed.stepCount = 11
    malformed.assertionCount = 12
    expect(() => validateSubflowContract(malformed)).toThrow('assertionCount')
    malformed.assertionCount = 5
    malformed.postconditions = [{ kind: 'screen-visible', resourceId: 'raw visible text' }]
    expect(() => validateSubflowContract(malformed)).toThrow('resourceId')
  })

  test('requires explicit non-new-device provenance and bounded curated artifacts', () => {
    const value = contract()
    value.provenance = {
      kind: 'curated-evidence', ticket: 'WALK-018', runId: 'fixture-run', newDeviceRunRecorded: false,
      artifacts: [{ path: 'evidence/login.yaml', sha256: 'b'.repeat(64) }],
    }
    expect(validateSubflowContract(value).provenance).toEqual(value.provenance)
    value.provenance.newDeviceRunRecorded = true as never
    expect(() => validateSubflowContract(value)).toThrow('exclude a new device run')
    value.provenance.newDeviceRunRecorded = false
    if (value.provenance.kind === 'curated-evidence') {
      value.provenance.artifacts[0].path = '../outside.yaml'
    }
    expect(() => validateSubflowContract(value)).toThrow('curated relative text evidence')
  })

  test('exact identity never crosses app, build, schema, adapter, or browser', () => {
    const base = contract().identity
    expect(sameSubflowIdentity(base, { ...base })).toBe(true)
    for (const altered of [
      { ...base, appId: 'other.app' },
      { ...base, buildFingerprint: 'b'.repeat(64) },
      { ...base, flowSchemaVersion: 'flow/v2' },
      { ...base, adapter: 'other' },
      { ...base, browser: 'other' },
    ]) expect(sameSubflowIdentity(base, altered)).toBe(false)
  })

  test('miss diagnoses remain exact and fail closed', () => {
    const base = contract()
    const segment: VerifiedSubflow = {
      ...base, flowPath: '.deja/segments/login-v1/fixture.yaml', flowSha256: 'c'.repeat(64),
      lastValidation: { passed: true, at: '2026-11-22T00:00:00.000Z' },
    }
    expect(subflowMissReason(base.identity, [])).toBe('not-found')
    expect(subflowMissReason({ ...base.identity, appId: 'other.app' }, [segment])).toBe('wrong-app')
    expect(subflowMissReason({ ...base.identity, platform: 'other' }, [segment])).toBe('wrong-platform')
    expect(subflowMissReason({ ...base.identity, flowSchemaVersion: 'other/v1' }, [segment]))
      .toBe('wrong-flow-schema')
    expect(subflowMissReason({ ...base.identity, buildFingerprint: 'd'.repeat(64) }, [segment]))
      .toBe('stale-build')
  })
})
