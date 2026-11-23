import { expect, test } from 'bun:test'
import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, realpath, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import {
  LOCAL_MEMORY_PATH,
  LocalWalkStore,
  MAESTRO_FLOW_SCHEMA_VERSION,
} from '@deja-qa/store-local'

const cli = resolve(import.meta.dir, '../../../spikes/001-android-deja/scripts/deja.ts')
const fakeMaestro = resolve(import.meta.dir, 'fixtures/fake-maestro.cjs')

async function execute(workspaceRoot: string) {
  const child = spawn(process.execPath, [cli, 'run', 'checkout'], {
    cwd: workspaceRoot,
    env: {
      ...process.env,
      DEJA_DEVICE: 'test-device',
      DEJA_FAKE_MAESTRO_MODE: 'selector',
      DEJA_MAESTRO: fakeMaestro,
      DEJA_OPENCLAW: join(workspaceRoot, 'model-must-not-be-invoked'),
      DEJA_TIMEOUT_MS: '2000',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  const stdout: string[] = []
  const stderr: string[] = []
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  child.stdout.on('data', chunk => stdout.push(chunk))
  child.stderr.on('data', chunk => stderr.push(chunk))
  const exitCode = await new Promise<number | null>(resolveExit => {
    child.once('error', error => stderr.push(String(error)))
    child.once('close', code => resolveExit(code))
  })
  const report = JSON.parse(await readFile(join(workspaceRoot, '.deja/reports/latest.json'), 'utf8'))
  return { exitCode, stdout: stdout.join(''), stderr: stderr.join(''), report }
}

test('a failing cache hit stays at zero model calls and its failed validation is not reused', async () => {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-cache-classification-')))
  const workspaceRoot = join(temporary, 'workspace')
  try {
    await mkdir(join(workspaceRoot, 'app/build/outputs/apk/debug'), { recursive: true })
    await mkdir(join(workspaceRoot, 'flows'), { recursive: true })
    const apk = Buffer.from('deterministic fake APK for cache orchestration')
    await writeFile(join(workspaceRoot, 'app/build/outputs/apk/debug/app-debug.apk'), apk)
    const buildFingerprint = createHash('sha256').update(apk).digest('hex')
    await writeFile(join(workspaceRoot, 'flows/candidate.yaml'), 'appId: dev.deja.checkout\n')
    const store = new LocalWalkStore({ workspaceRoot })
    const persisted = await store.persistVerifiedCandidate({
      identity: {
        goal: 'checkout',
        appId: 'dev.deja.checkout',
        platform: 'android',
        flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
        buildFingerprint,
      },
      candidatePath: 'flows/candidate.yaml',
      validation: { passed: true, at: '2026-11-22T00:00:00.000Z' },
      origin: 'remember',
    })
    const goodFlow = await readFile(join(workspaceRoot, persisted.flowPath), 'utf8')

    const failedHit = await execute(workspaceRoot)
    expect(failedHit.exitCode).toBe(1)
    expect(failedHit.stderr).toContain('Maestro execution error')
    expect(failedHit.report).toMatchObject({
      cacheStatus: 'hit', modelCalls: 0, passed: false,
      failureClassification: {
        category: 'selector', failingStep: 'Tap on id: checkout_button',
        advisory: true, repairEligible: true,
      },
    })
    expect(failedHit.report.failureClassification.rawArtifactPath)
      .toBe(failedHit.report.outputPath)
    expect(await readFile(join(workspaceRoot, failedHit.report.outputPath), 'utf8'))
      .toContain('Element not found: Id matching regex: checkout_button')
    const memory = JSON.parse(await readFile(join(workspaceRoot, LOCAL_MEMORY_PATH), 'utf8'))
    expect(memory.walks[0].lastValidation.passed).toBe(false)
    expect(memory.walks[0].validationHistory).toHaveLength(2)
    expect(await readFile(join(workspaceRoot, persisted.flowPath), 'utf8')).toBe(goodFlow)

    const failedValidationMiss = await execute(workspaceRoot)
    expect(failedValidationMiss.exitCode).toBe(2)
    expect(failedValidationMiss.report).toMatchObject({
      cacheStatus: 'miss', cacheMissReason: 'failed-validation', modelCalls: 0, passed: false,
    })
  } finally {
    await rm(temporary, { recursive: true, force: true })
  }
})
