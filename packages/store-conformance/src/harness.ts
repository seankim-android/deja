/*
 * The contract suites never import a test runner. A binding hands them `describe`, `test` and
 * `expect`: `bun:test` on Bun (`bun-binding.ts`) or `node:test` plus a small `expect`
 * (`node-binding.ts`). Called with no harness under Bun, a suite uses `bun:test` as before.
 */

export type Matchers = {
  toBe(expected: unknown): void
  toEqual(expected: unknown): void
  toMatchObject(expected: unknown): void
  toHaveLength(length: number): void
  toContain(item: unknown): void
  toMatch(pattern: RegExp): void
  toThrow(expected?: string | RegExp | (new (...args: never[]) => unknown)): void
  toBeInstanceOf(constructor: abstract new (...args: never[]) => unknown): void
  toBeGreaterThan(value: number): void
  toBeLessThan(value: number): void
  toBeUndefined(): void
  toBeDefined(): void
  toBeNull(): void
}

export type ExpectResult = Matchers & {
  not: Matchers
  rejects: Matchers
  resolves: Matchers
}

export type ContractExpect = {
  (actual: unknown, message?: string): ExpectResult
  /** Matches a string that matches `pattern`, anywhere inside an expected value. */
  stringMatching(pattern: RegExp): unknown
}

export type ContractHarness = {
  describe(name: string, body: () => void): void
  test(name: string, body: () => void | Promise<void>): void
  expect: ContractExpect
}

/** The runner of the current process: `bun:test` under Bun, otherwise the caller must pass one. */
const bunHarness: ContractHarness | undefined = typeof Bun === 'undefined'
  ? undefined
  : await import('bun:test').then(runner => runner as unknown as ContractHarness)

export function resolveHarness(harness?: ContractHarness): ContractHarness {
  const resolved = harness ?? bunHarness
  if (!resolved) throw new Error('Pass a ContractHarness (see @deja-qa/store-conformance node-binding) when not running under Bun.')
  return resolved
}
