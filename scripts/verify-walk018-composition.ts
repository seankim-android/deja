#!/usr/bin/env bun
import { createHash } from 'node:crypto'
import {
  mkdir, mkdtemp, realpath, readFile, readdir, rm, symlink, unlink, writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { Deja, type RunReporter, type WalkIdentity } from '@deja-qa/core'
import {
  MaestroWalkRunner,
  type MaestroExecutionAdapter,
  MaestroLoginComposer,
  androidMaestroAdapter,
  readMaestroContinuation,
} from '@deja-qa/maestro'
import {
  LOCAL_MEMORY_PATH,
  LocalVerifiedSubflowStore,
  LocalWalkStore,
  VERIFIED_SUBFLOW_INDEX_PATH,
  type SubflowContract,
} from '@deja-qa/store-local'

const repo = resolve(import.meta.dir, '..')
const evidenceRoot = 'spikes/001-android-deja/evidence/walk018'
const fixture = resolve(repo, 'packages/maestro/test/fixtures/composition-maestro.cjs')
const sourceDirectory = 'demo/live-evidence/android'
const acceptedBuild = '6aec5fea876938db14007ba3dbcb91f698832fe855886abd9dea1e7b28861759'
const acceptedRun = 'walk020-android-checkout-06'
const identity: WalkIdentity = {
  goal: 'Sign in and check out Mochi sticker pack',
  appId: 'dev.walkcache.checkout',
  platform: 'android',
  flowSchemaVersion: 'maestro/v1',
  buildFingerprint: acceptedBuild,
}

function sha256(contents: string | Buffer) {
  return createHash('sha256').update(contents).digest('hex')
}

async function read(relative: string) {
  return readFile(resolve(repo, relative), 'utf8')
}

function fixtureSummary(output: string) {
  const line = output.split(/\r?\n/).find(line => line.startsWith('FIXTURE_SUMMARY '))
  if (!line) throw new Error('Deterministic runner did not emit its execution summary')
  return JSON.parse(line.slice('FIXTURE_SUMMARY '.length))
}

async function executionOutput(workspaceRoot: string, artifacts: { kind: string; path: string }[]) {
  const output = artifacts.find(artifact => artifact.kind === 'output' && artifact.path.endsWith('output.log'))
  if (!output) throw new Error('Runner output artifact is missing')
  return readFile(resolve(workspaceRoot, output.path), 'utf8')
}

async function optionalTreeSnapshot(root: string) {
  const result: Record<string, { sha256: string; bytes: number }> = {}
  async function visit(directory: string, prefix: string): Promise<boolean> {
    let entries
    try {
      entries = await readdir(directory, { withFileTypes: true })
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === 'ENOENT' && prefix === '') return false
      throw error
    }
    for (const entry of entries.sort((left, right) => left.name.localeCompare(right.name))) {
      const path = join(directory, entry.name)
      const key = `${prefix}${entry.name}`
      if (entry.isDirectory()) {
        await visit(path, `${key}/`)
      } else if (entry.isFile()) {
        const bytes = await readFile(path)
        result[key] = { sha256: sha256(bytes), bytes: bytes.length }
      } else {
        throw new Error('Unexpected non-regular file in external canonical-output fixture')
      }
    }
    return true
  }
  return await visit(root, '') ? result : null
}

async function snapshot(workspaceRoot: string, segmentFlowPath: string, previousFlowPath: string) {
  const paths = [
    LOCAL_MEMORY_PATH,
    VERIFIED_SUBFLOW_INDEX_PATH,
    segmentFlowPath,
    previousFlowPath,
  ]
  const result: Record<string, { sha256: string; bytes: number }> = {}
  for (const path of paths) {
    const bytes = await readFile(resolve(workspaceRoot, path))
    result[path] = { sha256: sha256(bytes), bytes: bytes.length }
  }
  return result
}

const silentReporter: RunReporter = {
  report: async () => ({
    reportPath: '.deja/reports/fixture-report.json',
    latestPath: '.deja/reports/latest.json',
  }),
}

export async function runWalk018Verification() {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'walk018-composition-')))
  const workspaceRoot = join(temporary, 'workspace')
  const priorEnvironment = process.env.DEJA_COMPOSITION_FAIL
  try {
    await mkdir(join(workspaceRoot, '.deja/attempts'), { recursive: true })
    const contract: SubflowContract = JSON.parse(await read(`${evidenceRoot}/login-contract.json`))
    // The contract provenance and deterministic deriver bind this one known segment path.
    const loginSourceDirectory = resolve(repo, `${evidenceRoot}/segments/login-v1`)
    const loginNames = await readdir(loginSourceDirectory)
    if (loginNames.length !== 1 || !loginNames[0].endsWith('.yaml')) {
      throw new Error('Expected exactly one curated login segment')
    }
    const loginSource = `${evidenceRoot}/segments/login-v1/${loginNames[0]}`
    const loginCandidate = '.deja/attempts/login-segment.yaml'
    await writeFile(resolve(workspaceRoot, loginCandidate), await read(loginSource))

    const recordedSource = JSON.parse(await read(`${sourceDirectory}/cold-stored-memory.json`))
    // The recorded memory predates the Déjà rename and stores its flow under .walkcache/flows.
    // Seed the temporary workspace in today's .deja/flows layout; the evidence file is unchanged.
    const previousSource = {
      ...recordedSource,
      flowPath: String(recordedSource.flowPath).replace(/^\.walkcache\//, '.deja/'),
    }
    const previousFlowPath = previousSource.flowPath
    await mkdir(dirname(resolve(workspaceRoot, previousFlowPath)), { recursive: true })
    await writeFile(resolve(workspaceRoot, previousFlowPath), await read(
      `${sourceDirectory}/cold-saved-checkout.yaml`))
    await mkdir(dirname(resolve(workspaceRoot, LOCAL_MEMORY_PATH)), { recursive: true })
    await writeFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH), `${JSON.stringify({
      schemaVersion: 2,
      walks: [previousSource],
    }, null, 2)}\n`)
    const previousGoalMemory = await readFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH))

    const adapter: MaestroExecutionAdapter = androidMaestroAdapter({ device: 'deterministic-fixture' })
    const runner = new MaestroWalkRunner({
      workspaceRoot,
      executable: fixture,
      device: 'deterministic-fixture',
      timeoutMs: 30_000,
      adapter,
    })
    const segments = new LocalVerifiedSubflowStore({ workspaceRoot })
    const walks = new LocalWalkStore({ workspaceRoot })
    let artifactSequence = 0
    const composer = new MaestroLoginComposer({
      workspaceRoot, segments, walks, runner,
      artifactId: prefix => `${prefix}-${String(++artifactSequence).padStart(2, '0')}`,
    })
    const login = await composer.verifyAndSaveSegment(contract, loginCandidate)
    if (!login.segment || !login.passed) {
      throw new Error(`Login segment was not independently verified: ${JSON.stringify({
        errors: login.errors, failure: login.execution.failure, exitCode: login.execution.exitCode,
      })}`)
    }
    const loginOutput = await executionOutput(workspaceRoot, login.execution.artifacts)
    const segmentIndexBytes = await readFile(resolve(workspaceRoot, VERIFIED_SUBFLOW_INDEX_PATH))
    const segmentFlowBytes = await readFile(resolve(workspaceRoot, login.segment.flowPath))
    const continuationPath = `${evidenceRoot}/continuation/checkout.yaml`
    const continuation = await readMaestroContinuation(resolve(repo, continuationPath))
    const composed = await composer.compose(identity, 'login-v1', continuation)
    if (!composed.persisted || !composed.execution || !composed.steps || !composed.passed) {
      throw new Error(`Composition failed: ${composed.errors.join('; ')}`)
    }
    const composedOutput = await executionOutput(workspaceRoot, composed.execution.artifacts)
    // Persisted bytes below are deliberately read only after verification, just like LocalWalkStore.
    const persistedBytes = await readFile(resolve(workspaceRoot, composed.persisted.flowPath))
    if (sha256(persistedBytes) !== composed.persisted.flowSha256) {
      throw new Error('Persisted composition checksum mismatch')
    }
    const storedContents = persistedBytes.toString('utf8')
    for (const duplicated of ['screen_login', 'username_input', 'password_input', 'login_button',
      'qa@walkcache.dev', 'testing123']) {
      if (storedContents.includes(duplicated)) throw new Error('Stored composition duplicated login YAML')
    }

    let explorerConstructions = 0
    const cache = new Deja({
      store: walks, runner, reporter: silentReporter,
      artifactId: () => `exact-replay-${++artifactSequence}`,
      createExplorer: () => {
        explorerConstructions += 1
        throw new Error('Exact replay must not construct an explorer')
      },
    })
    const exact = await cache.run(identity)
    if (!exact.execution || exact.outcome !== 'passed') throw new Error('Stored exact replay failed')
    const exactOutput = await executionOutput(workspaceRoot, exact.execution.artifacts)
    const afterSuccess = await snapshot(workspaceRoot, login.segment.flowPath, previousFlowPath)
    const lastKnownGoodMemory = await readFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH))

    process.env.DEJA_COMPOSITION_FAIL = 'checkout-missing'
    const failed = await composer.compose(identity, 'login-v1', continuation)
    delete process.env.DEJA_COMPOSITION_FAIL
    if (failed.status !== 'rejected' || !failed.execution || failed.persisted !== null) {
      throw new Error('Failure fixture did not reject composition before persistence')
    }
    const failedOutput = await executionOutput(workspaceRoot, failed.execution.artifacts)
    const afterFailure = await snapshot(workspaceRoot, login.segment.flowPath, previousFlowPath)
    const afterFailureIndex = await readFile(resolve(workspaceRoot, VERIFIED_SUBFLOW_INDEX_PATH))
    const afterFailureSegment = await readFile(resolve(workspaceRoot, login.segment.flowPath))
    const afterFailureMemory = await readFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH))
    if (JSON.stringify(afterFailure) !== JSON.stringify(afterSuccess) ||
        !afterFailureIndex.equals(segmentIndexBytes) || !afterFailureSegment.equals(segmentFlowBytes) ||
        !afterFailureMemory.equals(lastKnownGoodMemory)) {
      throw new Error('Failed composition changed login bytes or last-known-good goal memory')
    }

    const wrongFinalContinuation = continuation.replace(
      'Order #WC-1001 · Mochi sticker pack', 'Order #WC-9999 · Wrong item')
    if (wrongFinalContinuation === continuation) throw new Error('Wrong-text mutation did not apply')
    const wrongFinalText = await composer.compose(identity, 'login-v1', wrongFinalContinuation)
    if (wrongFinalText.status !== 'rejected' || !wrongFinalText.execution ||
        wrongFinalText.execution.passed || wrongFinalText.persisted !== null ||
        wrongFinalText.execution.failure?.category !== 'product_assertion') {
      throw new Error('Wrong final text did not fail closed as a product assertion')
    }
    const wrongFinalTextOutput = await executionOutput(
      workspaceRoot, wrongFinalText.execution.artifacts)
    const afterWrongFinalText = await snapshot(
      workspaceRoot, login.segment.flowPath, previousFlowPath)
    if (JSON.stringify(afterWrongFinalText) !== JSON.stringify(afterSuccess) ||
        !(await readFile(resolve(workspaceRoot, VERIFIED_SUBFLOW_INDEX_PATH))).equals(segmentIndexBytes) ||
        !(await readFile(resolve(workspaceRoot, login.segment.flowPath))).equals(segmentFlowBytes) ||
        !(await readFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH))).equals(lastKnownGoodMemory)) {
      throw new Error('Wrong final text changed login bytes or last-known-good goal memory')
    }

    const lateLaunchCandidate = '.deja/attempts/login-second-clear-launch.yaml'
    await writeFile(resolve(workspaceRoot, lateLaunchCandidate), `${await read(loginSource)}- launchApp:\n    clearState: true\n`)
    // Keep all static counts valid so rejection must come from observations, not the old count gate.
    const lateLaunch = await composer.verifyAndSaveSegment(
      { ...contract, stepCount: contract.stepCount + 1 }, lateLaunchCandidate)
    if (lateLaunch.status !== 'rejected' || !lateLaunch.execution.passed ||
        lateLaunch.segment !== null || !lateLaunch.errors.some(error =>
          error.includes('exactly one clean launch'))) {
      throw new Error('Late second clear launch was not rejected by runner observations')
    }
    const lateLaunchOutput = await executionOutput(workspaceRoot, lateLaunch.execution.artifacts)
    const lateLaunchSummary = fixtureSummary(lateLaunchOutput)
    if (lateLaunchSummary.clearStateLaunches !== 2 || lateLaunchSummary.screen !== 'login') {
      throw new Error('Late-launch fixture did not actually complete a second clean launch')
    }
    const afterLateLaunch = await snapshot(workspaceRoot, login.segment.flowPath, previousFlowPath)
    if (JSON.stringify(afterLateLaunch) !== JSON.stringify(afterSuccess) ||
        !(await readFile(resolve(workspaceRoot, VERIFIED_SUBFLOW_INDEX_PATH))).equals(segmentIndexBytes) ||
        !(await readFile(resolve(workspaceRoot, login.segment.flowPath))).equals(segmentFlowBytes) ||
        !(await readFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH))).equals(lastKnownGoodMemory)) {
      throw new Error('Late second clear launch changed reusable or last-known-good memory')
    }

    const acceptedCandidatePath = composed.candidatePath
    const verifiedLoginSegment = login.segment
    if (!acceptedCandidatePath || !verifiedLoginSegment) {
      throw new Error('Accepted composition or login segment path is missing')
    }
    const candidateDirectory = dirname(resolve(workspaceRoot, acceptedCandidatePath))
    const composedRoot = dirname(candidateDirectory)
    let canonicalRunnerCalls = 0
    const canonicalComposer = new MaestroLoginComposer({
      workspaceRoot, segments, walks,
      runner: {
        run: async options => {
          canonicalRunnerCalls += 1
          return runner.run(options)
        },
      },
      artifactId: prefix => `${prefix}-canonical-${++artifactSequence}`,
    })
    const canonicalCases: {
      case: string
      rejectedBeforeRunner: boolean
      error: string
      externalBefore: unknown
      externalAfter: unknown
      externalUnchanged: boolean
      storageBefore: unknown
      storageAfter: unknown
      storageUnchanged: boolean
    }[] = []
    async function canonicalCase(
      name: string,
      externalRoot: string,
      install: () => Promise<void>,
      restore: () => Promise<void>,
    ) {
      const externalBefore = await optionalTreeSnapshot(externalRoot)
      await install()
      const storageBefore = await snapshot(workspaceRoot, verifiedLoginSegment.flowPath, previousFlowPath)
      try {
        let rejection: unknown = null
        try {
          await canonicalComposer.compose(identity, 'login-v1', continuation)
        } catch (error) {
          rejection = error
        }
        if (!rejection) throw new Error(`${name} reached execution instead of canonical preflight rejection`)
        if (canonicalRunnerCalls !== 0) throw new Error(`${name} invoked the runner`)
        const storageAfter = await snapshot(workspaceRoot, verifiedLoginSegment.flowPath, previousFlowPath)
        const storageUnchanged = JSON.stringify(storageAfter) === JSON.stringify(storageBefore) &&
          (await readFile(resolve(workspaceRoot, VERIFIED_SUBFLOW_INDEX_PATH))).equals(segmentIndexBytes) &&
          (await readFile(resolve(workspaceRoot, verifiedLoginSegment.flowPath))).equals(segmentFlowBytes) &&
          (await readFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH))).equals(lastKnownGoodMemory)
        if (!storageUnchanged) throw new Error(`${name} changed goal or segment storage`)
        const externalAfter = await optionalTreeSnapshot(externalRoot)
        const externalUnchanged = JSON.stringify(externalAfter) === JSON.stringify(externalBefore)
        if (!externalUnchanged) throw new Error(`${name} created or changed an external file`)
        canonicalCases.push({
          case: name,
          rejectedBeforeRunner: true,
          error: rejection instanceof Error ? rejection.message : String(rejection),
          externalBefore,
          externalAfter,
          externalUnchanged,
          storageBefore,
          storageAfter,
          storageUnchanged,
        })
      } finally {
        await restore()
      }
    }

    const externalGoal = join(temporary, 'outside-goal')
    await mkdir(externalGoal)
    await writeFile(join(externalGoal, 'candidate.yaml'), 'external goal sentinel\n')
    await canonicalCase('goal-directory-symlink', externalGoal, async () => {
      await rm(candidateDirectory, { recursive: true, force: true })
      await symlink(externalGoal, candidateDirectory, 'dir')
    }, async () => {
      await unlink(candidateDirectory)
      await mkdir(candidateDirectory, { recursive: true })
    })

    const externalComposed = join(temporary, 'outside-composed')
    // The exact goal directory name is derived by the composer; locate it from the accepted path.
    const goalDirectoryName = candidateDirectory.slice(composedRoot.length + 1)
    await mkdir(join(externalComposed, goalDirectoryName), { recursive: true })
    await writeFile(join(externalComposed, goalDirectoryName, 'candidate.yaml'), 'external root sentinel\n')
    await canonicalCase('composed-root-symlink', externalComposed, async () => {
      await rm(composedRoot, { recursive: true, force: true })
      await symlink(externalComposed, composedRoot, 'dir')
    }, async () => {
      await unlink(composedRoot)
      await mkdir(candidateDirectory, { recursive: true })
    })

    const externalDangling = join(temporary, 'outside-dangling')
    await canonicalCase('dangling-goal-directory-symlink', externalDangling, async () => {
      await rm(candidateDirectory, { recursive: true, force: true })
      await symlink(externalDangling, candidateDirectory, 'dir')
    }, async () => {
      await unlink(candidateDirectory)
      await mkdir(candidateDirectory, { recursive: true })
    })

    const externalNonDirectory = join(temporary, 'outside-non-directory')
    await canonicalCase('non-directory-goal-traversal', externalNonDirectory, async () => {
      await rm(candidateDirectory, { recursive: true, force: true })
      await writeFile(candidateDirectory, 'not a directory\n')
    }, async () => {
      await rm(candidateDirectory, { force: true })
      await mkdir(candidateDirectory, { recursive: true })
    })
    await canonicalCase('non-directory-composed-root-traversal', externalNonDirectory, async () => {
      await rm(composedRoot, { recursive: true, force: true })
      await writeFile(composedRoot, 'not a directory\n')
    }, async () => {
      await rm(composedRoot, { force: true })
      await mkdir(candidateDirectory, { recursive: true })
    })

    const previousIdentity: WalkIdentity = { ...identity, goal: previousSource.goal }
    const previousHit = await walks.lookup(previousIdentity)
    if (previousHit.status !== 'hit' || previousHit.walk.flowSha256 !== previousSource.flowSha256) {
      throw new Error('Previous accepted goal is no longer independently replayable')
    }

    const cleanSummary = fixtureSummary(composedOutput)
    const loginSummary = fixtureSummary(loginOutput)
    const exactSummary = fixtureSummary(exactOutput)
    if (!cleanSummary.finalProductAssertionPassed || cleanSummary.clearStateLaunches !== 1 ||
        cleanSummary.steps !== 22 || cleanSummary.reusedSteps !== 11 || cleanSummary.assertions !== 14 ||
        loginSummary.clearStateLaunches !== 1 || !exactSummary.finalProductAssertionPassed ||
        exactSummary.clearStateLaunches !== 1 || exact.assertions !== 14) {
      throw new Error(`Clean-state execution did not retain complete reused/new steps and assertions: ${JSON.stringify({ cleanSummary, loginSummary, exactSummary, exactAssertions: exact.assertions })}`)
    }
    const finalAssertionsFromObservations = (summary: {
      assertionObservations: { resourceId: string | null; text: string | null }[]
    }) => summary.assertionObservations.filter(observation =>
      observation.resourceId === 'order_confirmation' ||
      observation.resourceId === 'screen_confirmation')
    const retainedFinalAssertions = finalAssertionsFromObservations(cleanSummary)
    const replayedFinalAssertions = finalAssertionsFromObservations(exactSummary)
    if (retainedFinalAssertions.length !== 2 ||
        retainedFinalAssertions.some(observation => observation.text === null) ||
        JSON.stringify(replayedFinalAssertions) !== JSON.stringify(retainedFinalAssertions)) {
      throw new Error('Exactly two final product assertions must come from real runner observations')
    }
    const expectedComposed = await read(
      `${evidenceRoot}/composed/sign-in-and-check-out-mochi-sticker-pack/candidate.yaml`)
    if (storedContents !== expectedComposed) throw new Error('Runtime composition differs from curated YAML')
    const report = {
      schemaVersion: 1,
      ticket: 'WALK-018',
      generatedAt: new Date().toISOString(),
      newDeviceRunRecorded: false,
      provenance: {
        kind: 'curated-evidence', ticket: 'WALK-020', runId: acceptedRun,
        sourceFlowSha256: previousSource.flowSha256,
        sourceValidationAt: previousSource.lastValidation.at,
        buildFingerprint: acceptedBuild,
      },
      previousGoalMemorySha256: sha256(previousGoalMemory),
      loginSegment: {
        segmentId: login.segment.segmentId,
        flowPath: loginSource,
        storedFlowPath: login.segment.flowPath,
        flowSha256: login.segment.flowSha256,
        independentlyVerified: login.passed,
        verificationEndedAt: login.execution.endedAt,
        cleanState: loginSummary,
        inputs: contract.inputs,
        outputs: contract.outputs,
        preconditions: contract.preconditions,
        postconditions: contract.postconditions,
      },
      composition: {
        status: composed.status,
        goal: identity.goal,
        candidateSha256: composed.candidateSha256,
        storedFlowSha256: composed.persisted.flowSha256,
        verificationEndedAt: composed.execution.endedAt,
        steps: composed.steps,
        modelCalls: composed.modelCalls,
        cleanState: cleanSummary,
        finalProductAssertionsRetained: retainedFinalAssertions,
      },
      canonicalOutputContainment: {
        legitimateNewNestedOutput: {
          candidatePath: acceptedCandidatePath,
          candidateBytes: persistedBytes.length,
          candidateSha256: composed.candidateSha256,
        },
        runnerCallsDuringAdversarialCases: canonicalRunnerCalls,
        cases: canonicalCases,
      },
      exactCacheReplay: {
        cacheStatus: exact.cacheStatus,
        outcome: exact.outcome,
        modelCalls: exact.modelCalls,
        explorerConstructions,
        assertions: exact.assertions,
        cleanState: exactSummary,
        finalProductAssertionsRetained: replayedFinalAssertions,
      },
      failedCompositionPreservation: {
        status: failed.status,
        failureCategory: failed.execution.failure?.category,
        before: afterSuccess,
        after: afterFailure,
        byteForByteUnchanged: true,
        segmentIndexSha256: sha256(afterFailureIndex),
        segmentFlowSha256: sha256(afterFailureSegment),
        lastKnownGoodMemorySha256: sha256(afterFailureMemory),
        previousGoalStillUsable: true,
        finalProductAssertionsUnchanged: true,
        adversarialRegressions: {
          wrongFinalText: {
            status: wrongFinalText.status,
            runnerPassed: wrongFinalText.execution.passed,
            failureCategory: wrongFinalText.execution.failure?.category,
            before: afterSuccess,
            after: afterWrongFinalText,
            byteForByteUnchanged: true,
          },
          lateSecondClearLaunch: {
            status: lateLaunch.status,
            runnerPassed: lateLaunch.execution.passed,
            cleanStateLaunches: lateLaunchSummary.clearStateLaunches,
            finalScreen: lateLaunchSummary.screen,
            rejectionReasons: lateLaunch.errors,
            before: afterSuccess,
            after: afterLateLaunch,
            byteForByteUnchanged: true,
          },
        },
      },
      logs: {
        loginVerification: `${evidenceRoot}/walk018-login-verification.log`,
        cleanStateComposition: `${evidenceRoot}/walk018-clean-state-composition.log`,
        exactReplay: `${evidenceRoot}/walk018-exact-replay.log`,
        failedComposition: `${evidenceRoot}/walk018-failed-composition.log`,
        wrongFinalText: `${evidenceRoot}/walk018-wrong-final-text.log`,
        lateSecondClearLaunch: `${evidenceRoot}/walk018-late-second-clear-launch.log`,
        canonicalOutputContainment: `${evidenceRoot}/walk018-canonical-output-containment.log`,
      },
    }
    return {
      report,
      logs: {
        'walk018-login-verification.log': loginOutput,
        'walk018-clean-state-composition.log': composedOutput,
        'walk018-exact-replay.log': exactOutput,
        'walk018-failed-composition.log': failedOutput,
        'walk018-wrong-final-text.log': wrongFinalTextOutput,
        'walk018-late-second-clear-launch.log': lateLaunchOutput,
        'walk018-canonical-output-containment.log': `${canonicalCases.map(entry =>
          `CANONICAL_OUTPUT_CASE ${JSON.stringify({
            case: entry.case,
            rejectedBeforeRunner: entry.rejectedBeforeRunner,
            error: entry.error,
            externalBefore: entry.externalBefore,
            externalAfter: entry.externalAfter,
            externalUnchanged: entry.externalUnchanged,
            storageUnchanged: entry.storageUnchanged,
          })}`).join('\n')}\n`,
      },
    }
  } finally {
    if (priorEnvironment === undefined) delete process.env.DEJA_COMPOSITION_FAIL
    else process.env.DEJA_COMPOSITION_FAIL = priorEnvironment
    await rm(temporary, { recursive: true, force: true })
  }
}

if (import.meta.main) {
  const write = process.argv.includes('--write-evidence')
  const check = process.argv.includes('--check')
  if (write === check || process.argv.length > 3) throw new Error('Use exactly one of --check or --write-evidence')
  const { report, logs } = await runWalk018Verification()
  if (write) {
    for (const [name, contents] of Object.entries(logs)) {
      await writeFile(resolve(repo, evidenceRoot, name), contents)
    }
    await writeFile(resolve(repo, evidenceRoot, 'walk018-verification.json'),
      `${JSON.stringify(report, null, 2)}\n`)
  } else {
    const stable = (value: unknown) => JSON.parse(JSON.stringify(value))
    const expectedReport = stable(JSON.parse(await read(`${evidenceRoot}/walk018-verification.json`)))
    const actual = stable(report)
    // Runner wall-clock times enter validation metadata, so their derived hashes necessarily vary.
    // Compare them for equality within each fresh failure experiment above, but not across runs.
    for (const value of [actual, expectedReport]) {
      delete value.generatedAt
      delete value.loginSegment.verificationEndedAt
      delete value.composition.verificationEndedAt
      value.failedCompositionPreservation.before = 'validated-equal-within-run'
      value.failedCompositionPreservation.after = 'validated-equal-within-run'
      value.failedCompositionPreservation.segmentIndexSha256 = 'validated-byte-equal-within-run'
      value.failedCompositionPreservation.lastKnownGoodMemorySha256 = 'validated-byte-equal-within-run'
      for (const regression of Object.values(
        value.failedCompositionPreservation.adversarialRegressions) as {
          before: unknown
          after: unknown
        }[]) {
        regression.before = 'validated-equal-within-run'
        regression.after = 'validated-equal-within-run'
      }
      for (const entry of value.canonicalOutputContainment.cases) {
        entry.storageBefore = 'validated-equal-within-run'
        entry.storageAfter = 'validated-equal-within-run'
      }
    }
    if (JSON.stringify(actual) !== JSON.stringify(expectedReport)) {
      throw new Error('Fresh deterministic execution differs from curated WALK-018 verification evidence')
    }
    for (const [name, contents] of Object.entries(logs)) {
      if (await read(`${evidenceRoot}/${name}`) !== contents) {
        throw new Error(`${name} differs from fresh deterministic execution`)
      }
    }
  }
  console.log(JSON.stringify({
    result: 'pass', mode: write ? 'write-evidence' : 'check', ticket: 'WALK-018',
    newDeviceRunRecorded: false,
    provenance: report.provenance,
    reusedSteps: report.composition.steps.reusedSteps,
    newlyExploredSteps: report.composition.steps.newlyExploredSteps,
    totalAssertions: report.composition.cleanState.assertions,
    modelCalls: report.composition.modelCalls,
    exactCacheReplay: report.exactCacheReplay,
    failedCompositionPreservedByteForByte:
      report.failedCompositionPreservation.byteForByteUnchanged,
    adversarialRegressions: report.failedCompositionPreservation.adversarialRegressions,
    canonicalOutputContainment: report.canonicalOutputContainment,
  }, null, 2))
}
