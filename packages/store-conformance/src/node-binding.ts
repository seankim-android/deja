import { AssertionError } from 'node:assert/strict'
import { describe, test } from 'node:test'
import { inspect, isDeepStrictEqual } from 'node:util'
import type { ContractExpect, ContractHarness, ExpectResult, Matchers } from './harness.ts'

/*
 * `node:test` plus a small `expect`, so the contract suites run on Node with no dependency. It
 * implements only the matchers the suites use, with the same meaning as `bun:test`.
 */

class Asymmetric {
  readonly matches: (actual: unknown) => boolean
  readonly label: string
  constructor(matches: (actual: unknown) => boolean, label: string) {
    this.matches = matches
    this.label = label
  }
}

/** Deep equality like `toEqual`: asymmetric matchers allowed, `undefined` properties ignored. */
function equals(actual: unknown, expected: unknown): boolean {
  if (expected instanceof Asymmetric) return expected.matches(actual)
  if (actual === expected) return true
  if (typeof actual !== 'object' || typeof expected !== 'object' || actual === null || expected === null) {
    return Object.is(actual, expected)
  }
  if (ArrayBuffer.isView(actual) || ArrayBuffer.isView(expected)) return isDeepStrictEqual(actual, expected)
  if (Array.isArray(actual) !== Array.isArray(expected)) return false
  if (Array.isArray(actual)) {
    return actual.length === (expected as unknown[]).length && actual.every((item, index) => equals(item, (expected as unknown[])[index]))
  }
  const left = actual as Record<string, unknown>
  const right = expected as Record<string, unknown>
  const keys = new Set([...Object.keys(left), ...Object.keys(right)])
  for (const key of keys) {
    if (left[key] === undefined && right[key] === undefined) continue
    if (!equals(left[key], right[key])) return false
  }
  return true
}

/** `toMatchObject`: every expected property matches; extra actual properties are fine. */
function matchesObject(actual: unknown, expected: unknown): boolean {
  if (expected instanceof Asymmetric) return expected.matches(actual)
  if (typeof expected !== 'object' || expected === null) return Object.is(actual, expected)
  if (typeof actual !== 'object' || actual === null) return false
  if (Array.isArray(expected)) {
    return Array.isArray(actual) && actual.length === expected.length && expected.every((item, index) => matchesObject(actual[index], item))
  }
  return Object.entries(expected).every(([key, value]) => matchesObject((actual as Record<string, unknown>)[key], value))
}

function thrownMatches(error: unknown, expected: unknown): boolean {
  if (expected === undefined) return true
  if (typeof expected === 'string') return String((error as { message?: unknown })?.message ?? error).includes(expected)
  if (expected instanceof RegExp) return expected.test(String((error as { message?: unknown })?.message ?? error))
  return typeof expected === 'function' && error instanceof (expected as new (...args: never[]) => unknown)
}

type Name = keyof Matchers

const checks: Record<Exclude<Name, 'toThrow'>, (actual: unknown, expected: never) => boolean> = {
  toBe: (actual, expected) => Object.is(actual, expected),
  toEqual: (actual, expected) => equals(actual, expected),
  toMatchObject: (actual, expected) => matchesObject(actual, expected),
  toHaveLength: (actual, length: number) => (actual as { length?: number } | null)?.length === length,
  toContain: (actual, item) => typeof actual === 'string'
    ? actual.includes(item as string)
    : [...(actual as Iterable<unknown>)].some(value => Object.is(value, item)),
  toMatch: (actual, pattern: RegExp) => typeof actual === 'string' && pattern.test(actual),
  toBeInstanceOf: (actual, constructor: abstract new (...args: never[]) => unknown) => actual instanceof constructor,
  toBeGreaterThan: (actual, value: number) => (actual as number) > value,
  toBeLessThan: (actual, value: number) => (actual as number) < value,
  toBeUndefined: actual => actual === undefined,
  toBeDefined: actual => actual !== undefined,
  toBeNull: actual => actual === null,
}
const names = [...Object.keys(checks), 'toThrow'] as Name[]

function evaluate(name: Name, negated: boolean, actual: unknown, args: unknown[], called: boolean, label?: string) {
  let passed: boolean
  if (name === 'toThrow') {
    // `actual` is a function to call, or (after `rejects`) the error it already produced.
    if (called) {
      passed = thrownMatches(actual, args[0])
    } else {
      let thrown: unknown = NO_VALUE
      try { (actual as () => unknown)() } catch (error) { thrown = error }
      passed = thrown !== NO_VALUE && thrownMatches(thrown, args[0])
    }
  } else {
    passed = (checks[name] as (actual: unknown, ...args: never[]) => boolean)(actual, ...(args as never[]))
  }
  if (passed === negated) {
    const shown = args.map(arg => arg instanceof Asymmetric ? arg.label : inspect(arg, { depth: 6, breakLength: 120 })).join(', ')
    throw new AssertionError({
      message: `${label ? `${label}: ` : ''}expected ${inspect(actual, { depth: 6, breakLength: 120 })}${negated ? ' not' : ''} ${name}(${shown})`,
      actual, operator: name,
    })
  }
}

const NO_VALUE = Symbol('no value')

function matchers(negated: boolean, run: (name: Name, negated: boolean, args: unknown[]) => unknown): Matchers {
  return Object.fromEntries(names.map(name => [name, (...args: unknown[]) => run(name, negated, args)])) as unknown as Matchers
}

function createExpect(): ContractExpect {
  const expect = ((actual: unknown, message?: string): ExpectResult => {
    const settled = () => Promise.resolve(actual).then(value => ({ value }), (error: unknown) => ({ error }))
    return {
      ...matchers(false, (name, negated, args) => evaluate(name, negated, actual, args, false, message)),
      not: matchers(true, (name, negated, args) => evaluate(name, negated, actual, args, false, message)),
      rejects: matchers(false, async (name, negated, args) => {
        const result = await settled()
        if ('value' in result) throw new AssertionError({ message: `expected a rejection, got ${inspect(result.value)}`, operator: name })
        evaluate(name, negated, result.error, args, true, message)
      }),
      resolves: matchers(false, async (name, negated, args) => {
        const result = await settled()
        if ('error' in result) throw result.error
        evaluate(name, negated, result.value, args, false, message)
      }),
    }
  }) as ContractExpect
  expect.stringMatching = (pattern: RegExp) => new Asymmetric(actual => typeof actual === 'string' && pattern.test(actual), `stringMatching(${String(pattern)})`)
  return expect
}

/** The Node binding: pass it as the second argument of any `run…ContractTests` function. */
export const nodeHarness: ContractHarness = {
  describe: (name, body) => { describe(name, body) },
  test: (name, body) => { test(name, async () => { await body() }) },
  expect: createExpect(),
}
