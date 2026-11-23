import { describe, expect, test } from 'bun:test'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, realpath, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import type { MaestroFailureCategory, MaestroRunResult } from '@deja-qa/maestro'
import {
  LOCAL_MEMORY_PATH,
  LocalWalkStore,
  MAESTRO_FLOW_SCHEMA_VERSION,
} from '@deja-qa/store-local'
import { BoundedCheckoutRepairer } from '../src/index.ts'
import type { RepairBuildResult } from '../src/build.ts'
import { CHECKOUT_AFFECTED_FILES } from '../src/policy.ts'

const oldBuild = 'b'.repeat(64)
const diagnosisBuild = 'a'.repeat(64)
const repairedBuild = 'c'.repeat(64)
const timestamp = '2026-11-22T00:00:00.000Z'
const verifiedTimestamp = '2026-11-22T00:02:00.000Z'
const checkoutStep = 'Tap on id: dev.deja.checkout:id/checkout_button'
const finalAssertion = 'Assert that "Order #WC-1001 · Mochi sticker pack" is visible... FAILED'

const staleComponent = `final class MainActivity {
    void showCart() {
        Button checkout = button("Checkout", null);
        checkout.setOnClickListener(view -> showConfirmation());
    }
}
`
const staleIds = `<?xml version="1.0" encoding="utf-8"?>
<resources>
    <item name="screen_cart" type="id" />
    <item name="screen_confirmation" type="id" />
    <!-- checkout_button is deliberately absent for the selector-repair spike. -->
</resources>
`
const flow = `appId: dev.deja.checkout
---
- launchApp:
    clearState: true
- tapOn:
    id: dev.deja.checkout:id/checkout_button
- assertVisible: Order confirmed
- assertVisible: "Order #WC-1001 · Mochi sticker pack"
`

function runnerResult(
  category: MaestroFailureCategory | 'passed' | 'other-selector',
  startedAt = timestamp,
): MaestroRunResult {
  const passed = category === 'passed'
  let output = 'Launch app "dev.deja.checkout" with clear state... COMPLETED\n'
  let timedOut = false
  let errors: MaestroRunResult['errors'] = []
  let exitCode: number | null = 0
  if (!passed) {
    exitCode = 1
    errors = [{ kind: 'execution', message: 'Maestro exited with code 1' }]
  }
  if (category === 'selector') {
    output += `${checkoutStep}... FAILED\nElement not found: Id matching regex: checkout_button\n`
  } else if (category === 'other-selector') {
    output += 'Tap on id: another_button... FAILED\nElement not found: Id matching regex: another_button\n'
  } else if (category === 'product_assertion') {
    output += `${finalAssertion}\nAssertion is false: Order #WC-1001 · Mochi sticker pack\n`
  } else if (category === 'navigation') {
    output += 'Launch app "dev.deja.checkout"... FAILED\nUnable to launch app: dev.deja.checkout\n'
  } else if (category === 'timeout') {
    timedOut = true
    exitCode = null
    errors = [{ kind: 'timeout', message: 'Maestro exceeded budget' }]
    output += `${checkoutStep}... FAILED\nElement not found: Id matching regex: checkout_button\n`
  } else if (category === 'runner') {
    exitCode = null
    errors = [{ kind: 'launch', message: 'Maestro executable is missing' }]
  } else if (category === 'unknown') {
    output += 'An ambiguous failure with no single supported diagnostic\n'
  }
  return {
    platform: 'android',
    device: 'test-device',
    flowPath: '/unused.yaml',
    startedAt,
    endedAt: category === 'passed' ? verifiedTimestamp : timestamp,
    durationMs: 10,
    monotonicDurationMs: 10,
    exitCode,
    signal: null,
    timedOut,
    passed,
    stdout: output,
    stderr: '',
    output,
    errors,
  }
}

function buildResult(passed = true): RepairBuildResult {
  return {
    command: ['/fixture/gradlew', 'clean', ':app:assembleDebug'],
    startedAt: timestamp,
    endedAt: timestamp,
    durationMs: 5,
    monotonicDurationMs: 5,
    exitCode: passed ? 0 : 1,
    signal: null,
    timedOut: false,
    passed,
    stdout: passed ? 'BUILD SUCCESSFUL\n' : '',
    stderr: passed ? '' : 'BUILD FAILED\n',
    output: passed ? 'BUILD SUCCESSFUL\n' : 'BUILD FAILED\n',
    errors: passed ? [] : ['Gradle exited with code 1'],
  }
}

async function fixture() {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-repair-')))
  const root = join(temporary, 'workspace')
  await mkdir(join(root, 'app/src/main/java/dev/deja/checkout'), { recursive: true })
  await mkdir(join(root, 'app/src/main/res/values'), { recursive: true })
  await writeFile(join(root, CHECKOUT_AFFECTED_FILES[0]), staleComponent)
  await writeFile(join(root, CHECKOUT_AFFECTED_FILES[1]), staleIds)
  await mkdir(join(root, 'candidate'), { recursive: true })
  await writeFile(join(root, 'candidate/checkout.yaml'), flow)
  const store = new LocalWalkStore({ workspaceRoot: root })
  const persisted = await store.persistVerifiedCandidate({
    identity: {
      goal: 'checkout',
      appId: 'dev.deja.checkout',
      platform: 'android',
      flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
      buildFingerprint: oldBuild,
    },
    candidatePath: 'candidate/checkout.yaml',
    validation: { passed: true, at: timestamp },
    origin: 'remember',
  })
  const policy = {
    workspaceRoot: root,
    sourceRoots: [
      'app/src/main/java/dev/deja/checkout',
      'app/src/main/res/values',
    ],
    permittedFileTypes: ['.java', '.xml'],
    affectedFiles: [...CHECKOUT_AFFECTED_FILES],
  }
  return { temporary, root, store, persisted, policy }
}

async function cleanup(temporary: string) {
  await rm(temporary, { recursive: true, force: true })
}

async function sourceSnapshot(root: string) {
  return Promise.all(CHECKOUT_AFFECTED_FILES.map(path => readFile(join(root, path))))
}

async function runRepair(
  context: Awaited<ReturnType<typeof fixture>>,
  firstResult: MaestroRunResult,
  overrides: {
    timeoutMs?: number
    verificationResult?: MaestroRunResult
    sameBuildAfterRepair?: boolean
    failingBuild?: boolean
    hangingBuild?: boolean
  } = {},
) {
  let fingerprint = diagnosisBuild
  let diagnosisCalls = 0
  let verificationCalls = 0
  let buildCalls = 0
  let displayedDiff = ''
  const repairer = new BoundedCheckoutRepairer({
    workspaceRoot: context.root,
    goal: 'checkout',
    appId: 'dev.deja.checkout',
    platform: 'android',
    timeoutMs: overrides.timeoutMs ?? 30_000,
    policy: context.policy,
    attemptId: 'repair-focused-test',
    store: context.store,
    memoryPath: LOCAL_MEMORY_PATH,
    fingerprint: async () => fingerprint,
    runFlow: async (_flowPath, _artifactDirectory, timeoutMs) => {
      expect(timeoutMs).toBeGreaterThan(0)
      diagnosisCalls += 1
      if (diagnosisCalls === 1) return firstResult
      verificationCalls += 1
      return overrides.verificationResult ?? runnerResult('passed')
    },
    build: async _remainingMs => {
      buildCalls += 1
      if (overrides.hangingBuild) return new Promise<RepairBuildResult>(() => {})
      if (!overrides.sameBuildAfterRepair) fingerprint = repairedBuild
      return buildResult(!overrides.failingBuild)
    },
    displayPatch: diff => { displayedDiff = diff },
  })
  const report = await repairer.repair()
  return {
    repairer,
    report,
    displayedDiff,
    counts: () => ({ diagnosisCalls, verificationCalls, buildCalls }),
  }
}

describe('bounded checkout selector repair', () => {
  test('verifies the exact two-file patch, new build identity, no Git action, and atomic replacement', async () => {
    const context = await fixture()
    const originalPath = process.env.PATH
    const gitMarker = join(context.temporary, 'git-invoked')
    const fakeBin = join(context.temporary, 'bin')
    await mkdir(fakeBin, { recursive: true })
    await writeFile(join(fakeBin, 'git'), `#!/bin/sh\ntouch "${gitMarker}"\nexit 99\n`, { mode: 0o755 })
    process.env.PATH = `${fakeBin}:${originalPath ?? ''}`
    try {
      const beforeMemory = await readFile(join(context.root, LOCAL_MEMORY_PATH))
      const beforeFlow = await readFile(join(context.root, context.persisted.flowPath))
      const result = await runRepair(context, runnerResult('selector'))
      expect(result.report.status).toBe('repaired')
      expect(result.report.passed).toBe(true)
      expect(result.report.repairAttempts).toBe(1)
      expect(result.report.oneAttemptOnly).toBe(true)
      expect(result.report.autoCommit).toBe(false)
      expect(result.report.pullRequest).toBe(false)
      expect(result.report.affectedFiles).toEqual([...CHECKOUT_AFFECTED_FILES])
      expect(result.report.diagnosisBuildFingerprint).toBe(diagnosisBuild)
      expect(result.report.buildFingerprintAfter).toBe(repairedBuild)
      expect(result.report.buildIdentityRecomputed).toBe(true)
      expect(result.report.build?.passed).toBe(true)
      expect(result.report.verification?.passed).toBe(true)
      expect(result.report.persistence).toMatchObject({
        atomicWrite: true,
        origin: 'repair',
        preservedValidationHistoryEntries: 2,
      })
      expect(result.counts()).toEqual({ diagnosisCalls: 2, verificationCalls: 1, buildCalls: 1 })
      expect(existsSync(gitMarker)).toBe(false)
      expect(result.displayedDiff).toBe(await readFile(join(context.root, result.report.proposedPatchPath!), 'utf8'))
      expect(result.displayedDiff).toContain('checkout_button')
      expect(result.displayedDiff).not.toContain('Order #WC-9999')
      expect(result.displayedDiff.split('\n').filter(line => line.startsWith('--- a/'))).toEqual(
        CHECKOUT_AFFECTED_FILES.map(path => `--- a/${path}`),
      )
      const source = await sourceSnapshot(context.root)
      expect(source[0].toString()).toContain('Button checkout = button("Checkout", checkoutId);')
      expect(source[1].toString()).toContain('<item name="checkout_button" type="id" />')
      expect(await readFile(join(context.root, context.persisted.flowPath))).toEqual(beforeFlow)
      const memory = JSON.parse(await readFile(join(context.root, LOCAL_MEMORY_PATH), 'utf8'))
      expect(memory.walks[0].origin).toBe('repair')
      expect(memory.walks[0].validationHistory).toHaveLength(2)
      expect(memory.walks[0].repairHistory[0]).toMatchObject({
        attemptId: 'repair-focused-test',
        previousBuildFingerprint: oldBuild,
      })
      expect((await context.store.lookup({
        goal: 'checkout', appId: 'dev.deja.checkout', platform: 'android',
        flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION, buildFingerprint: repairedBuild,
      })).status).toBe('hit')
      expect(beforeMemory.equals(await readFile(join(context.root, result.report.attemptDirectory, 'previous/walks.json')))).toBe(true)
      expect(existsSync(join(context.root, result.report.attemptDirectory, 'build/build-report.json'))).toBe(true)
      expect(existsSync(join(context.root, result.report.attemptDirectory, 'verification/verification-report.json'))).toBe(true)
      await expect(result.repairer.repair()).rejects.toThrow('already consumed its one attempt')
      expect(result.counts()).toEqual({ diagnosisCalls: 2, verificationCalls: 1, buildCalls: 1 })
    } finally {
      process.env.PATH = originalPath
      await cleanup(context.temporary)
    }
  })

  for (const category of [
    'product_assertion', 'navigation', 'unknown', 'timeout', 'runner', 'other-selector',
  ] as const) {
    test(`refuses ${category} without source, assertion, build, or memory changes`, async () => {
      const context = await fixture()
      try {
        const beforeSource = await sourceSnapshot(context.root)
        const beforeFlow = await readFile(join(context.root, context.persisted.flowPath))
        const beforeMemory = await readFile(join(context.root, LOCAL_MEMORY_PATH))
        const result = await runRepair(context, runnerResult(category))
        expect(result.report.status).toBe('rejected')
        expect(result.report.passed).toBe(false)
        expect(result.report.repairAttempts).toBe(0)
        expect(result.report.proposedPatchPath).toBe(null)
        expect(result.report.build).toBe(null)
        expect(result.report.verification).toBe(null)
        expect(result.counts()).toEqual({ diagnosisCalls: 1, verificationCalls: 0, buildCalls: 0 })
        expect(result.displayedDiff).toBe('')
        expect(await sourceSnapshot(context.root)).toEqual(beforeSource)
        expect(await readFile(join(context.root, context.persisted.flowPath))).toEqual(beforeFlow)
        expect(await readFile(join(context.root, LOCAL_MEMORY_PATH))).toEqual(beforeMemory)
        expect(result.report.preservation).toMatchObject({
          memoryUnchangedByteForByte: true,
          assertionsUnchangedByteForByte: true,
          sourceUnchangedByteForByte: true,
        })
        expect(result.report.rejectionReason).toContain('Repair refused')
      } finally {
        await cleanup(context.temporary)
      }
    })
  }

  test('failed full-journey verification rolls back source and preserves last-known-good memory and failed artifacts', async () => {
    const context = await fixture()
    try {
      const beforeSource = await sourceSnapshot(context.root)
      const beforeMemory = await readFile(join(context.root, LOCAL_MEMORY_PATH))
      const result = await runRepair(
        context,
        runnerResult('selector'),
        { verificationResult: runnerResult('product_assertion') },
      )
      expect(result.report.status).toBe('failed')
      expect(result.report.repairAttempts).toBe(1)
      expect(result.report.persistence).toBe(null)
      expect(result.report.verification?.failureClassification?.category).toBe('product_assertion')
      expect(result.report.preservation).toMatchObject({
        memoryUnchangedByteForByte: true,
        sourceUnchangedByteForByte: true,
        assertionsUnchangedByteForByte: true,
      })
      expect(await sourceSnapshot(context.root)).toEqual(beforeSource)
      expect(await readFile(join(context.root, LOCAL_MEMORY_PATH))).toEqual(beforeMemory)
      expect(existsSync(join(context.root, result.report.proposedPatchPath!))).toBe(true)
      expect(existsSync(join(context.root, result.report.attemptDirectory, 'verification/output.log'))).toBe(true)
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('rejects an unchanged rebuilt fingerprint before verification or persistence', async () => {
    const context = await fixture()
    try {
      const beforeMemory = await readFile(join(context.root, LOCAL_MEMORY_PATH))
      const beforeSource = await sourceSnapshot(context.root)
      const result = await runRepair(context, runnerResult('selector'), { sameBuildAfterRepair: true })
      expect(result.report.status).toBe('failed')
      expect(result.report.error).toContain('fingerprint did not change')
      expect(result.report.buildFingerprintAfter).toBe(diagnosisBuild)
      expect(result.counts()).toEqual({ diagnosisCalls: 1, verificationCalls: 0, buildCalls: 1 })
      expect(result.report.preservation.memoryUnchangedByteForByte).toBe(true)
      expect(result.report.preservation.sourceUnchangedByteForByte).toBe(true)
      expect(await readFile(join(context.root, LOCAL_MEMORY_PATH))).toEqual(beforeMemory)
      expect(await sourceSnapshot(context.root)).toEqual(beforeSource)
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('enforces one overall time budget and rolls back a partial attempt without retrying', async () => {
    const context = await fixture()
    try {
      const beforeSource = await sourceSnapshot(context.root)
      const beforeMemory = await readFile(join(context.root, LOCAL_MEMORY_PATH))
      const result = await runRepair(
        context,
        runnerResult('selector'),
        { timeoutMs: 40, hangingBuild: true },
      )
      expect(result.report.status).toBe('failed')
      expect(result.report.error).toContain('time budget expired during bounded rebuild')
      expect(result.report.repairAttempts).toBe(1)
      expect(result.counts()).toEqual({ diagnosisCalls: 1, verificationCalls: 0, buildCalls: 1 })
      expect(result.report.preservation).toMatchObject({
        memoryUnchangedByteForByte: true,
        sourceUnchangedByteForByte: true,
      })
      expect(await sourceSnapshot(context.root)).toEqual(beforeSource)
      expect(await readFile(join(context.root, LOCAL_MEMORY_PATH))).toEqual(beforeMemory)
    } finally {
      await cleanup(context.temporary)
    }
  })
})
