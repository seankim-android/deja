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
  /** IDs of a hidden page that leaks into the hierarchy before the screen's own elements. */
  ghost?: string[]
  /** Show the platform's software keyboard on top. */
  keyboard?: boolean
  /** Add this many extra anonymous elements with IDs, to cross the 400 bound. */
  extra?: number
}

export type ExplorationDriverContractImplementation = {
  name: string
  /** `appId` for `reset`. */
  appId: string
  /** Normalized IDs that make the driver offer at least one back action. Empty when none can. */
  backIds: string[]
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

    test('observe and act never relaunch; reset launches once; the same screen gives the same fingerprint', async () => {
      const { driver, relaunches } = implementation.create(base)
      const profile = { appId: implementation.appId, arguments: {}, clearState: true }
      expect((await driver.reset(profile, [])).passed).toBe(true)
      expect(relaunches()).toBe(1)
      const first = await driver.observe()
      for (let i = 0; i < 3; i++) {
        expect((await driver.act({ label: 'tap', yaml: '- tapOn:\n    id: "x"\n' })).passed).toBe(true)
        await driver.observe()
      }
      const second = await driver.observe()
      expect(relaunches()).toBe(1)
      expect(first.snapshot.sha256).toBe(second.snapshot.sha256)
      expect(fingerprintScreenV2(first.elements, implementation.fingerprint).fingerprint)
        .toBe(fingerprintScreenV2(second.elements, implementation.fingerprint).fingerprint)
    })

    test('act refuses a segment that relaunches or rebinds the app', async () => {
      const { driver } = implementation.create(base)
      await driver.reset({ appId: implementation.appId, arguments: {}, clearState: true }, [])
      for (const yaml of ['- launchApp\n', '- launchApp:\n    clearState: true\n', 'appId: other.app\n---\n- tapOn: x\n', '- openLink: x://y\n']) {
        await expect(driver.act({ label: 'bad', yaml })).rejects.toThrow()
      }
    })

    test('a hidden page that leaks in makes the screen a gap, unless markerScope last-marker is set', async () => {
      const clean = await fingerprintOf(base)
      const ghosted = await fingerprintOf({ ...base, ghost: ['screen.journal', 'journal.you'] })
      expect(ghosted.fingerprint).toMatchObject({ stable: false, unstableReason: 'marker-conflict' })
      const scoped = fingerprintScreenV2(ghosted.observation.elements, { ...implementation.fingerprint, markerScope: 'last-marker' })
      expect(scoped.stable).toBe(true)
      expect(scoped.fingerprint).toBe(clean.fingerprint.fingerprint)
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

    test('back actions are launch-free segments, and exist where the platform has a back control', async () => {
      const { driver } = implementation.create({ ...base, ids: [...base.ids, ...implementation.backIds, 'x.sheet'] })
      const observation = await driver.observe()
      const actions = driver.backActions(observation)
      if (implementation.backIds.length > 0) expect(actions.length).toBeGreaterThan(0)
      for (const action of actions) {
        expect(action.segment.yaml).not.toContain('launchApp')
        expect(action.segment.yaml.length).toBeGreaterThan(0)
      }
    })
  })
}
