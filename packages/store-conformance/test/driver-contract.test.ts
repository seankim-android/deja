import {
  normalizeElements,
  normalizedSnapshot,
  type ExplorationDriver,
  type FingerprintRules,
  type RawElement,
} from '@deja-qa/core'
import {
  iosExplorationDriver,
  type MaestroFlowResult,
  type MaestroTransport,
} from '@deja-qa/maestro'
import {
  runExplorationDriverContractTests,
  type ContractScene,
  type ExplorationDriverContractImplementation,
} from '../src/index.ts'

const PASSED: MaestroFlowResult = { passed: true, exitCode: 0, output: '', durationMs: 1, timedOut: false }
const FAILED: MaestroFlowResult = { passed: false, exitCode: 1, output: 'Element not found', durationMs: 1, timedOut: false }

function sceneIds(scene: ContractScene, keyboardKeys: string[]) {
  return [
    ...(scene.ghost ?? []),
    ...scene.ids,
    ...Array.from({ length: scene.extra ?? 0 }, (_, index) => `extra.item.${index}x`),
    ...(scene.keyboard ? keyboardKeys : []),
  ]
}

// ---- A fake driver with no platform at all: proves the suite tests the contract, not iOS.

const fakeRules: FingerprintRules = {
  normalizeId: raw => raw || null,
  isOverlay: element => element.rawId?.startsWith('kbd.') ?? false,
  isSystemUi: () => false,
}

const fake: ExplorationDriverContractImplementation = {
  name: 'fake driver',
  appId: 'dev.deja.fake',
  backIds: [],
  fingerprint: { screenMarkerPattern: '^screen\\.' },
  create(scene) {
    let launches = 0
    let failNext = false
    const element = (rawId: string | null, label: string | null): RawElement => ({
      rawId, label, bounds: null, enabled: true, focused: false, selected: false, checked: false,
      role: 'other', interactive: false,
    })
    const raw = [
      ...sceneIds(scene, ['kbd.space', 'kbd.a']).map(id => element(id, null)),
      ...(scene.labels ?? []).map(label => element(null, label)),
    ]
    const driver: ExplorationDriver = {
      platform: 'web',
      rules: fakeRules,
      async reset() {
        launches += 1
        return { passed: true, failure: null, durationMs: 1, crashed: false }
      },
      async observe() {
        const { elements, truncated } = normalizeElements(raw, fakeRules)
        const keyboardVisible = !!scene.keyboard
        const snapshot = normalizedSnapshot({ elements, keyboardVisible, truncated })
        return {
          elements, truncated, keyboardVisible, systemUi: null,
          snapshot: { path: '', sha256: snapshot.sha256 }, capturedAt: '2026-11-22T00:00:00.000Z', durationMs: 1,
        }
      },
      async act(segment) {
        if (/launchApp|openLink|^appId:|\nappId:/.test(segment.yaml)) throw new TypeError('segment relaunches the app')
        if (failNext) {
          failNext = false
          return {
            passed: false, durationMs: 1, crashed: false,
            failure: { category: 'selector', advisory: true, repairEligible: false, failingStep: 'tap', rawArtifactPath: null },
          }
        }
        return { passed: true, failure: null, durationMs: 1, crashed: false }
      },
      backActions: () => [],
      systemUi: observation => observation.keyboardVisible ? { kind: 'keyboard', evidence: ['kbd.space'] } : null,
    }
    return { driver, relaunches: () => launches, failNextAct: () => { failNext = true } }
  },
}

// ---- The iOS driver over a fake transport that serves `maestro hierarchy` JSON.

function hierarchyJson(ids: string[], labels: string[]) {
  const node = (id: string, label: string) => ({
    attributes: {
      accessibilityText: label, title: '', value: '', text: '', hintText: '', 'resource-id': id,
      bounds: '[0,0][10,10]', enabled: 'true', focused: 'false', selected: 'false', checked: 'false',
    },
    children: [],
  })
  return {
    attributes: { 'resource-id': '', accessibilityText: 'App', bounds: '[0,0][402,874]' },
    children: [...ids.map(id => node(id, '')), ...labels.map(label => node('', label))],
  }
}

const ios: ExplorationDriverContractImplementation = {
  name: 'iosExplorationDriver',
  appId: 'dev.deja.ios',
  backIds: ['BackButton'],
  fingerprint: { screenMarkerPattern: '^screen\\.' },
  create(scene) {
    let launches = 0
    let failNext = false
    const transport: MaestroTransport = {
      kind: 'process',
      async hierarchy() {
        const keys = ['space', 'return', 'shift', 'a', 'b']
        return { raw: hierarchyJson(sceneIds(scene, keys), scene.labels ?? []), durationMs: 1 }
      },
      async runFlow(yaml) {
        if (yaml.includes('launchApp')) launches += 1
        if (failNext) {
          failNext = false
          return FAILED
        }
        return PASSED
      },
      async close() {},
    }
    return { driver: iosExplorationDriver({ transport }), relaunches: () => launches, failNextAct: () => { failNext = true } }
  },
}

runExplorationDriverContractTests(fake)
runExplorationDriverContractTests(ios)
