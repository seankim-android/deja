import { describe, expect, test } from 'bun:test'
import {
  MAX_OBSERVED_ELEMENTS,
  fingerprintScreenV2,
  type ExplorationDriver,
  type FingerprintConfig,
} from '@deja-qa/core'

/** A screen described the same way for every platform. The implementation turns it into a hierarchy. */
export type ContractScene = {
  /** Stable IDs on the screen. */
  ids: string[]
  /** Visible labels with no ID. They must never change a fingerprint. */
  labels?: string[]
  /** Show the platform's software keyboard on top. */
  keyboard?: boolean
  /** Add this many extra anonymous elements with IDs, to cross the 400 bound. */
  extra?: number
}

export type ExplorationDriverContractImplementation = {
  name: string
  /** `appId` for `reset`. */
  appId: string
  fingerprint: FingerprintConfig
  /**
   * A driver whose `observe()` returns `scene`. `relaunches()` counts launches since creation;
   * `failNextAct()` makes the next `act` fail.
   */
  create(scene: ContractScene): {
    driver: ExplorationDriver
    relaunches(): number
    failNextAct(): void
  }
}

/**
 * Shared behavioural suite for every `ExplorationDriver` (iOS now; Android reuses it in DEJA-20's
 * parity work). Every assertion is the same for every platform.
 */
export function runExplorationDriverContractTests(implementation: ExplorationDriverContractImplementation) {
  const base: ContractScene = { ids: ['screen.today', 'today.answer', 'today.save', 'tab.feed'], labels: ['Hello'] }
  const fingerprintOf = async (scene: ContractScene) => {
    const { driver } = implementation.create(scene)
    const observation = await driver.observe()
    return { observation, fingerprint: fingerprintScreenV2(observation.elements, implementation.fingerprint) }
  }

  describe(`${implementation.name}: ExplorationDriver contract`, () => {
    test('declares a platform and rules', () => {
      const { driver } = implementation.create(base)
      expect(['ios', 'android', 'web']).toContain(driver.platform)
      expect(typeof driver.rules.normalizeId).toBe('function')
    })

    test('observe never relaunches and the same screen gives the same fingerprint', async () => {
      const { driver, relaunches } = implementation.create(base)
      const first = await driver.observe()
      const second = await driver.observe()
      expect(relaunches()).toBe(0)
      expect(first.snapshot.sha256).toBe(second.snapshot.sha256)
      expect(fingerprintScreenV2(first.elements, implementation.fingerprint).fingerprint)
        .toBe(fingerprintScreenV2(second.elements, implementation.fingerprint).fingerprint)
    })

    test('the keyboard is an overlay: same fingerprint, keyboardVisible, system UI reported', async () => {
      const plain = await fingerprintOf(base)
      const typing = await fingerprintOf({ ...base, keyboard: true })
      expect(plain.observation.keyboardVisible).toBe(false)
      expect(typing.observation.keyboardVisible).toBe(true)
      expect(typing.fingerprint.fingerprint).toBe(plain.fingerprint.fingerprint)
      const { driver } = implementation.create({ ...base, keyboard: true })
      expect(driver.systemUi(typing.observation)?.kind).toBe('keyboard')
      expect(driver.systemUi(plain.observation)).toBeNull()
    })

    test('visible text never changes a fingerprint', async () => {
      const one = await fingerprintOf({ ...base, labels: ['Hello'] })
      const other = await fingerprintOf({ ...base, labels: ['Bonjour', 'Another line'] })
      expect(other.fingerprint.fingerprint).toBe(one.fingerprint.fingerprint)
    })

    test('a screen with no stable IDs is not stable', async () => {
      const { fingerprint } = await fingerprintOf({ ids: [], labels: ['Only text'] })
      expect(fingerprint.stable).toBe(false)
    })

    test('observations are bounded and say so', async () => {
      const small = await fingerprintOf(base)
      expect(small.observation.truncated).toBe(false)
      const large = await fingerprintOf({ ...base, extra: MAX_OBSERVED_ELEMENTS + 50 })
      expect(large.observation.truncated).toBe(true)
      expect(large.observation.elements.length).toBe(MAX_OBSERVED_ELEMENTS)
    })

    test('act reports a failed step as passed: false with a failure, never a throw', async () => {
      const { driver, failNextAct } = implementation.create(base)
      await driver.reset({ appId: implementation.appId, arguments: {}, clearState: true }, [])
      failNextAct()
      const result = await driver.act({ label: 'tap', yaml: '- tapOn:\n    id: "x"\n' })
      expect(result.passed).toBe(false)
      expect(result.failure).not.toBeNull()
      expect(result.crashed).toBe(false)
    })

    test('back actions are launch-free segments', async () => {
      const { driver } = implementation.create({ ...base, ids: [...base.ids, 'BackButton', 'x.sheet'] })
      const observation = await driver.observe()
      for (const action of driver.backActions(observation)) {
        expect(action.segment.yaml).not.toContain('launchApp')
        expect(action.segment.yaml.length).toBeGreaterThan(0)
      }
    })
  })
}
