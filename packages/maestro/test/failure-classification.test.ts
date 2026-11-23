import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import {
  classifyMaestroFailure,
  type MaestroFailureCategory,
  type MaestroFailureInput,
} from '../src/index.ts'

type FixtureCase = {
  name: string
  category: MaestroFailureCategory
  failingStep: string | null
  timedOut?: boolean
  errors?: MaestroFailureInput['errors']
}

const selectorStep = 'Tap on id: checkout_button'
const navigationStep = 'Launch app "dev.deja.checkout" with clear state'
const productStep = 'Assert that "Order #WC-9999" is visible'
const execution = [{ kind: 'execution' } as const]

const cases: FixtureCase[] = [
  { name: 'selector-positive', category: 'selector', failingStep: selectorStep },
  { name: 'selector-negative', category: 'product_assertion', failingStep: productStep },
  { name: 'navigation-positive', category: 'navigation', failingStep: navigationStep },
  { name: 'navigation-negative', category: 'selector', failingStep: selectorStep },
  { name: 'product-assertion-positive', category: 'product_assertion', failingStep: productStep },
  {
    name: 'product-assertion-negative',
    category: 'unknown',
    failingStep: 'Assert that id: order_confirmation is visible',
  },
  {
    name: 'runner-positive', category: 'runner', failingStep: null,
    errors: [{ kind: 'launch' }],
  },
  { name: 'runner-negative', category: 'product_assertion', failingStep: productStep },
  {
    name: 'timeout-positive', category: 'timeout', failingStep: selectorStep,
    timedOut: true, errors: [{ kind: 'timeout' }],
  },
  { name: 'timeout-negative', category: 'selector', failingStep: selectorStep },
  {
    name: 'unknown-positive', category: 'unknown',
    failingStep: 'Perform a proprietary fixture action',
  },
  { name: 'unknown-negative', category: 'navigation', failingStep: navigationStep },
]

describe('advisory Maestro failure classification fixtures', () => {
  for (const fixture of cases) {
    test(`${fixture.name} remains deterministic and conservative`, async () => {
      const rawArtifactPath = `artifacts/${fixture.name}.log`
      const output = await readFile(resolve(
        import.meta.dir,
        `fixtures/classification/${fixture.name}.log`,
      ), 'utf8')
      const classification = classifyMaestroFailure({
        passed: false,
        timedOut: fixture.timedOut ?? false,
        output,
        errors: fixture.errors ?? execution,
        rawArtifactPath,
      })
      expect(classification).toEqual({
        category: fixture.category,
        failingStep: fixture.failingStep,
        rawArtifactPath,
        advisory: true,
        repairEligible: fixture.category === 'selector' || fixture.category === 'navigation',
      })
      if (classification?.category === 'unknown' || classification?.category === 'product_assertion') {
        expect(classification.repairEligible).toBe(false)
      }
    })
  }

  test('a passing runner result has no failure classification', () => {
    expect(classifyMaestroFailure({
      passed: true, timedOut: false, output: '', errors: [], rawArtifactPath: 'output.log',
    })).toBeNull()
  })

  test('mixed selector and assertion diagnostics stay unknown and ineligible for repair', () => {
    expect(classifyMaestroFailure({
      passed: false,
      timedOut: false,
      output: `${selectorStep}... FAILED\nElement not found: id\nAssertion is false: id is visible\n`,
      errors: execution,
      rawArtifactPath: 'output.log',
    })).toMatchObject({
      category: 'unknown', failingStep: selectorStep, repairEligible: false,
    })
  })

  test('multiple failing steps stay ambiguous and retain the raw artifact reference', () => {
    expect(classifyMaestroFailure({
      passed: false,
      timedOut: false,
      output: `${selectorStep}... FAILED\n${productStep}... FAILED\nElement not found: id\n`,
      errors: execution,
      rawArtifactPath: 'output.log',
    })).toMatchObject({
      category: 'unknown', failingStep: null, rawArtifactPath: 'output.log', repairEligible: false,
    })
  })
})
