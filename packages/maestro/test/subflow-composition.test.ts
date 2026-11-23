import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import type { ExecutionResult, VerifiedSubflowStore, WalkIdentity } from '../../core/src/index.ts'
import {
  MaestroLoginComposer,
  MaestroWalkRunner,
  androidMaestroAdapter,
  composeMaestroLoginFlow,
  countFlowAssertions,
  readMaestroContinuation,
} from '../src/index.ts'
import { LocalVerifiedSubflowStore, LocalWalkStore } from '../../store-local/src/index.ts'
import { runWalk018Verification } from '../../../scripts/verify-walk018-composition.ts'

const repo = resolve(import.meta.dir, '../../..')
const evidence = 'spikes/001-android-deja/evidence/walk018'
const identity: WalkIdentity = {
  goal: 'Sign in and check out Mochi sticker pack',
  appId: 'dev.walkcache.checkout', platform: 'android', flowSchemaVersion: 'maestro/v1',
  buildFingerprint: '6aec5fea876938db14007ba3dbcb91f698832fe855886abd9dea1e7b28861759',
}

function execution(passed: boolean): ExecutionResult {
  return {
    passed, startedAt: '2026-11-22T00:00:00.000Z', endedAt: '2026-11-22T00:00:01.000Z',
    durationMs: 1000, exitCode: passed ? 0 : 1, assertions: passed ? 11 : 3,
    failure: passed ? null : {
      category: 'product_assertion', advisory: true, repairEligible: false,
      failingStep: 'Assert that "fixture" is visible', rawArtifactPath: null,
    }, artifacts: [],
  }
}

describe('bounded verified login composition', () => {
  test('pure composition contains a reference and all new assertions, never login commands or values', async () => {
    const continuation = await readFile(resolve(repo, `${evidence}/continuation/checkout.yaml`), 'utf8')
    const workspaceRoot = resolve(repo, evidence)
    const yaml = composeMaestroLoginFlow(
      identity.appId, '../../segments/login-v1/android-fixture.yaml', continuation, {
        workspaceRoot,
        flowDirectory: resolve(workspaceRoot, 'composed/sign-in-and-check-out-mochi-sticker-pack'),
      })
    expect((yaml.match(/^- runFlow:/gm) ?? [])).toHaveLength(1)
    expect(yaml).toContain('- runFlow: "../../segments/login-v1/android-fixture.yaml"')
    for (const duplicated of [
      'launchApp', 'screen_login', 'username_input', 'password_input', 'login_button',
      'qa@walkcache.dev', 'testing123',
    ]) expect(yaml).not.toContain(duplicated)
    expect(yaml).toContain('id: order_confirmation')
    expect(yaml).toContain('id: screen_confirmation')
    expect(yaml).toContain('Order #WC-1001 · Mochi sticker pack')
  })

  test('flow references accept same-directory files and reject every containment escape', () => {
    const workspaceRoot = resolve(repo, evidence)
    const continuation = '- assertVisible: Catalog\n'
    const containment = { workspaceRoot, flowDirectory: workspaceRoot }
    expect(composeMaestroLoginFlow(identity.appId, 'login.yaml', continuation, containment))
      .toContain('- runFlow: "login.yaml"')
    expect(composeMaestroLoginFlow(
      identity.appId, 'flows/../login.yaml', continuation, containment))
      .toContain('- runFlow: "login.yaml"')
    for (const reference of [
      '../../../outside.yaml',
      'flows/../../../outside.yaml',
      '/tmp/outside.yaml',
      '..\\outside.yaml',
    ]) {
      expect(() => composeMaestroLoginFlow(identity.appId, reference, continuation, containment))
        .toThrow(/relative path|escapes containment/)
    }
    expect(() => composeMaestroLoginFlow(
      identity.appId, 'login.yaml', continuation,
      { workspaceRoot, flowDirectory: resolve(workspaceRoot, '../outside') }))
      .toThrow('Flow directory escapes containment root')
  })

  test('rejects nested launch, nested composition, headers, and duplicated login controls', async () => {
    const valid = await readMaestroContinuation(resolve(repo, `${evidence}/continuation/checkout.yaml`))
    expect(valid).toContain('- tapOn:\n    id: checkout_button')
    for (const invalid of [
      '- launchApp:\n    clearState: true\n',
      '- runFlow: "../login.yaml"\n',
      'appId: fixture.app\n---\n- assertVisible: Catalog\n',
      '- tapOn:\n    id: login_button\n',
      '- inputText: qa@walkcache.dev\n- tapOn:\n    id: username_input\n',
    ]) {
      expect(() => composeMaestroLoginFlow(identity.appId, 'login.yaml', invalid))
        .toThrow()
    }
  })

  test('assertion accounting follows reused runFlow references and detects cycles', async () => {
    const composed = resolve(repo,
      `${evidence}/composed/sign-in-and-check-out-mochi-sticker-pack/candidate.yaml`)
    expect(await countFlowAssertions(composed)).toBe(14)
    const temporary = await mkdtemp(join(tmpdir(), 'walk018-count-'))
    try {
      await writeFile(join(temporary, 'a.yaml'), '- runFlow: "b.yaml"\n- assertVisible: A\n')
      await writeFile(join(temporary, 'b.yaml'), '- runFlow: "a.yaml"\n- assertVisible: B\n')
      expect(await countFlowAssertions(join(temporary, 'a.yaml'))).toBeNull()
      await writeFile(join(temporary, 'b.yaml'), '- assertVisible: B\n')
      expect(await countFlowAssertions(join(temporary, 'a.yaml'))).toBe(2)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('failed independent login verification never calls reusable segment persistence', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'walk018-login-reject-'))
    try {
      const workspaceRoot = temporary
      await writeFile(join(workspaceRoot, 'placeholder'), 'root')
      const contract = JSON.parse(await readFile(resolve(repo, `${evidence}/login-contract.json`), 'utf8'))
      let runnerCalls = 0
      const segments = new LocalVerifiedSubflowStore({ workspaceRoot })
      let persistCalls = 0
      const spySegments: VerifiedSubflowStore = {
        lookup: (segmentId, exactIdentity) => segments.lookup(segmentId, exactIdentity),
        list: () => segments.list(),
        persistVerified: async () => {
          persistCalls += 1
          throw new Error('must not persist')
        },
      }
      const composer = new MaestroLoginComposer({
        workspaceRoot, segments: spySegments, walks: new LocalWalkStore({ workspaceRoot }),
        runner: { run: async () => { runnerCalls += 1; return execution(false) } },
        artifactId: prefix => prefix,
      })
      const result = await composer.verifyAndSaveSegment(contract, 'candidate.yaml')
      expect(result.status).toBe('rejected')
      expect(result.passed).toBe(false)
      expect(result.segment).toBeNull()
      expect(runnerCalls).toBe(1)
      expect(persistCalls).toBe(0)
      expect(await segments.list()).toEqual([])
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('a passing run cannot save a segment when runner assertions disagree with its contract', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'walk018-contract-reject-'))
    try {
      const workspaceRoot = temporary
      const candidatePath = 'login.yaml'
      const loginName = 'android-c415a62dae8b1346213ac1f92c5201d2a8c17e01d98c84e6ab979f4d785f3266.yaml'
      await writeFile(resolve(workspaceRoot, candidatePath), await readFile(resolve(
        repo, `${evidence}/segments/login-v1/${loginName}`), 'utf8'))
      const contract = JSON.parse(await readFile(resolve(repo, `${evidence}/login-contract.json`), 'utf8'))
      const segments = new LocalVerifiedSubflowStore({ workspaceRoot })
      const composer = new MaestroLoginComposer({
        workspaceRoot, segments, walks: new LocalWalkStore({ workspaceRoot }),
        runner: { run: async () => execution(true) },
      })
      const result = await composer.verifyAndSaveSegment(contract, candidatePath)
      expect(result.status).toBe('rejected')
      expect(result.errors).toContain('Runner assertion count differs from the typed contract')
      expect(result.segment).toBeNull()
      expect(await segments.list()).toEqual([])
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('a passing run without runner observations cannot authorize reusable persistence', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'walk018-observation-reject-'))
    try {
      const workspaceRoot = temporary
      const candidatePath = 'login.yaml'
      const loginName = 'android-c415a62dae8b1346213ac1f92c5201d2a8c17e01d98c84e6ab979f4d785f3266.yaml'
      await writeFile(resolve(workspaceRoot, candidatePath), await readFile(resolve(
        repo, `${evidence}/segments/login-v1/${loginName}`), 'utf8'))
      const contract = JSON.parse(await readFile(resolve(repo, `${evidence}/login-contract.json`), 'utf8'))
      const segments = new LocalVerifiedSubflowStore({ workspaceRoot })
      const composer = new MaestroLoginComposer({
        workspaceRoot, segments, walks: new LocalWalkStore({ workspaceRoot }),
        runner: { run: async () => ({ ...execution(true), assertions: contract.assertionCount }) },
      })
      const result = await composer.verifyAndSaveSegment(contract, candidatePath)
      expect(result.status).toBe('rejected')
      expect(result.errors).toContain(
        'Runner did not expose bounded observations required for segment verification')
      expect(result.segment).toBeNull()
      expect(await segments.list()).toEqual([])
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('unavailable exact segment identity fails closed before execution or goal persistence', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'walk018-wrong-build-'))
    try {
      const workspaceRoot = temporary
      await writeFile(join(workspaceRoot, 'placeholder'), 'root')
      let runnerCalls = 0
      const composer = new MaestroLoginComposer({
        workspaceRoot, segments: new LocalVerifiedSubflowStore({ workspaceRoot }),
        walks: new LocalWalkStore({ workspaceRoot }),
        runner: { run: async () => { runnerCalls += 1; return execution(true) } },
      })
      const continuation = await readFile(resolve(repo, `${evidence}/continuation/checkout.yaml`), 'utf8')
      const result = await composer.compose(identity, 'login-v1', continuation)
      expect(result.status).toBe('rejected')
      expect(result.errors[0]).toContain('not-found')
      expect(result.candidatePath).toBeNull()
      expect(runnerCalls).toBe(0)
      expect(await new LocalWalkStore({ workspaceRoot }).list()).toEqual([])
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('goal-store replacement failure retains memory and verified login bytes after a passing run', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'walk018-persist-failure-'))
    const priorFailure = process.env.DEJA_COMPOSITION_FAIL
    delete process.env.DEJA_COMPOSITION_FAIL
    try {
      const workspaceRoot = temporary
      const loginName = 'android-c415a62dae8b1346213ac1f92c5201d2a8c17e01d98c84e6ab979f4d785f3266.yaml'
      const candidatePath = '.deja/attempts/login-segment.yaml'
      await mkdir(dirname(resolve(workspaceRoot, candidatePath)), { recursive: true })
      await writeFile(resolve(workspaceRoot, candidatePath), await readFile(resolve(
        repo, `${evidence}/segments/login-v1/${loginName}`), 'utf8'))
      const recorded = JSON.parse(await readFile(resolve(
        repo, 'demo/live-evidence/android/cold-stored-memory.json'), 'utf8'))
      // The recorded memory predates the Déjà rename (.walkcache/flows); seed today's layout.
      const previous = { ...recorded, flowPath: recorded.flowPath.replace(/^\.walkcache\//, '.deja/') }
      await mkdir(dirname(resolve(workspaceRoot, previous.flowPath)), { recursive: true })
      await writeFile(resolve(workspaceRoot, previous.flowPath), await readFile(resolve(
        repo, 'demo/live-evidence/android/cold-saved-checkout.yaml'), 'utf8'))
      const memoryPath = resolve(workspaceRoot, '.deja/memory/walks.json')
      await mkdir(dirname(memoryPath), { recursive: true })
      await writeFile(memoryPath, `${JSON.stringify({ schemaVersion: 2, walks: [previous] }, null, 2)}\n`)
      const runner = new MaestroWalkRunner({
        workspaceRoot, executable: resolve(repo, 'packages/maestro/test/fixtures/composition-maestro.cjs'),
        device: 'deterministic-fixture', timeoutMs: 5_000,
        adapter: androidMaestroAdapter({ device: 'deterministic-fixture' }),
      })
      const segments = new LocalVerifiedSubflowStore({ workspaceRoot })
      const failingWalks = new LocalWalkStore({
        workspaceRoot,
        renameFile: async (temporaryPath, destinationPath) => {
          if (destinationPath === memoryPath) throw new Error('injected goal metadata failure')
          await rename(temporaryPath, destinationPath)
        },
      })
      const composer = new MaestroLoginComposer({
        workspaceRoot, segments, walks: failingWalks, runner,
        artifactId: prefix => `${prefix}-persistence-failure`,
      })
      const contract = JSON.parse(await readFile(resolve(repo, `${evidence}/login-contract.json`), 'utf8'))
      const login = await composer.verifyAndSaveSegment(contract, candidatePath)
      expect(login.passed).toBe(true)
      if (!login.segment) throw new Error('expected verified login')
      const memoryBefore = await readFile(memoryPath)
      const indexBefore = await readFile(segments.indexPath)
      const segmentBefore = await readFile(resolve(workspaceRoot, login.segment.flowPath))
      const previousBefore = await readFile(resolve(workspaceRoot, previous.flowPath))
      const continuation = await readFile(resolve(repo, `${evidence}/continuation/checkout.yaml`), 'utf8')
      const result = await composer.compose(identity, 'login-v1', continuation)
      expect(result).toMatchObject({
        status: 'persist-failed', passed: false, verificationPassed: true, persisted: null,
        steps: { totalSteps: 22, totalAssertions: 14 },
      })
      expect(result.execution?.passed).toBe(true)
      expect(result.execution?.assertions).toBe(14)
      expect(await readFile(memoryPath)).toEqual(memoryBefore)
      expect(await readFile(segments.indexPath)).toEqual(indexBefore)
      expect(await readFile(resolve(workspaceRoot, login.segment.flowPath))).toEqual(segmentBefore)
      expect(await readFile(resolve(workspaceRoot, previous.flowPath))).toEqual(previousBefore)
      const normalWalks = new LocalWalkStore({ workspaceRoot })
      expect(await normalWalks.lookup(identity)).toEqual({ status: 'miss', reason: 'not-found' })
      expect((await normalWalks.lookup({ ...identity, goal: previous.goal })).status).toBe('hit')
    } finally {
      if (priorFailure === undefined) delete process.env.DEJA_COMPOSITION_FAIL
      else process.env.DEJA_COMPOSITION_FAIL = priorFailure
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('executable clean-state, exact-cache, canonical containment, and preservation evidence matches curated YAML', async () => {
    const { report, logs } = await runWalk018Verification()
    expect(report.newDeviceRunRecorded).toBe(false)
    expect(report.provenance).toMatchObject({
      ticket: 'WALK-020', runId: 'walk020-android-checkout-06',
      sourceFlowSha256: 'e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1',
      buildFingerprint: identity.buildFingerprint,
    })
    expect(report.loginSegment).toMatchObject({
      segmentId: 'login-v1', independentlyVerified: true,
      flowSha256: 'c415a62dae8b1346213ac1f92c5201d2a8c17e01d98c84e6ab979f4d785f3266',
      cleanState: { clearStateLaunches: 1, steps: 11, assertions: 5, screen: 'catalog' },
    })
    expect(report.composition).toMatchObject({
      status: 'composed',
      candidateSha256: '59778cf6a4dd43d2cac304e2af0145fd6ffde695031e8154221832f5295908ab',
      storedFlowSha256: '59778cf6a4dd43d2cac304e2af0145fd6ffde695031e8154221832f5295908ab',
      steps: {
        reusedSteps: 11, newlyExploredSteps: 11, totalSteps: 22,
        reusedAssertions: 5, newAssertions: 9, totalAssertions: 14,
      },
      modelCalls: { composition: 0, newExploration: 0, segmentVerification: 0, fullJourneyVerification: 0 },
      cleanState: {
        clearStateLaunches: 1, steps: 22, reusedSteps: 11, assertions: 14,
        finalProductAssertionPassed: true, screen: 'confirmation',
      },
    })
    expect(report.exactCacheReplay).toMatchObject({
      cacheStatus: 'hit', outcome: 'passed', modelCalls: 0, explorerConstructions: 0, assertions: 14,
      cleanState: { clearStateLaunches: 1, finalProductAssertionPassed: true },
    })
    expect(report.canonicalOutputContainment).toMatchObject({
      legitimateNewNestedOutput: {
        candidateBytes: 701,
        candidateSha256: '59778cf6a4dd43d2cac304e2af0145fd6ffde695031e8154221832f5295908ab',
      },
      runnerCallsDuringAdversarialCases: 0,
    })
    expect(report.canonicalOutputContainment.cases.map(entry => entry.case)).toEqual([
      'goal-directory-symlink',
      'composed-root-symlink',
      'dangling-goal-directory-symlink',
      'non-directory-goal-traversal',
      'non-directory-composed-root-traversal',
    ])
    for (const entry of report.canonicalOutputContainment.cases) {
      expect(entry).toMatchObject({
        rejectedBeforeRunner: true, externalUnchanged: true, storageUnchanged: true,
      })
      expect(entry.externalAfter).toEqual(entry.externalBefore)
      expect(entry.storageAfter).toEqual(entry.storageBefore)
    }
    expect(report.failedCompositionPreservation).toMatchObject({
      status: 'rejected', failureCategory: 'selector', byteForByteUnchanged: true,
      previousGoalStillUsable: true, finalProductAssertionsUnchanged: true,
    })
    expect(report.failedCompositionPreservation.before)
      .toEqual(report.failedCompositionPreservation.after)
    const regressions = report.failedCompositionPreservation.adversarialRegressions
    expect(regressions.wrongFinalText).toMatchObject({
      status: 'rejected', runnerPassed: false, failureCategory: 'product_assertion',
      byteForByteUnchanged: true,
    })
    expect(regressions.wrongFinalText.before).toEqual(regressions.wrongFinalText.after)
    expect(regressions.lateSecondClearLaunch).toMatchObject({
      status: 'rejected', runnerPassed: true, cleanStateLaunches: 2, finalScreen: 'login',
      byteForByteUnchanged: true,
    })
    expect(regressions.lateSecondClearLaunch.rejectionReasons.join(' '))
      .toContain('exactly one clean launch')
    expect(regressions.lateSecondClearLaunch.before)
      .toEqual(regressions.lateSecondClearLaunch.after)
    expect(report.composition.finalProductAssertionsRetained).toEqual([
      { resourceId: 'order_confirmation', text: 'Order confirmed' },
      { resourceId: 'screen_confirmation', text: 'Order #WC-1001 · Mochi sticker pack' },
    ])
    expect(report.exactCacheReplay.finalProductAssertionsRetained)
      .toEqual(report.composition.finalProductAssertionsRetained)
    expect(logs['walk018-login-verification.log']).toContain('clear state... COMPLETED')
    const retainedFinalText =
      'id: screen_confirmation is visible, text: "Order #WC-1001 · Mochi sticker pack"... COMPLETED'
    expect(logs['walk018-clean-state-composition.log']).toContain(retainedFinalText)
    expect(logs['walk018-exact-replay.log']).toContain(retainedFinalText)
    expect(logs['walk018-failed-composition.log']).toContain('Element not found: Id matching regex: checkout_button')
    expect(logs['walk018-failed-composition.log']).not.toContain(retainedFinalText)
    expect(logs['walk018-wrong-final-text.log']).toContain('Order #WC-9999 · Wrong item')
    expect(logs['walk018-wrong-final-text.log']).toContain('"finalProductAssertionPassed":false')
    expect(logs['walk018-late-second-clear-launch.log']).toContain('"clearStateLaunches":2')
    expect(logs['walk018-late-second-clear-launch.log']).toContain('"screen":"login"')
    const canonicalLog = logs['walk018-canonical-output-containment.log']
    expect(canonicalLog.split('\n').filter(line => line.startsWith('CANONICAL_OUTPUT_CASE '))).toHaveLength(5)
    expect(canonicalLog).toContain('"case":"goal-directory-symlink"')
    expect(canonicalLog).toContain('15e74983ad4e975d79b682a10279d8799d221ff9cf36b7c6e245167b75856c16')
    expect(canonicalLog).toContain('"case":"composed-root-symlink"')
    expect(canonicalLog).toContain('3ed2becd50eb90b8396a3441717e1d142cce9bfdffdfa98fd561dbd7b7aee014')
  })
})
