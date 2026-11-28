#!/usr/bin/env bun
import { createHash } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import { composeMaestroLoginFlow } from '@deja-run/maestro'
import { validateSubflowContract } from '@deja-run/core'

const repo = resolve(import.meta.dir, '..')
const sourceDirectory = 'demo/live-evidence/android'
const evidenceDirectory = 'spikes/001-android-deja/evidence/walk018'
const acceptedFlowSha256 = 'e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1'
const buildFingerprint = '6aec5fea876938db14007ba3dbcb91f698832fe855886abd9dea1e7b28861759'
const runId = 'walk020-android-checkout-06'
const write = process.argv.includes('--write')
if (process.argv.length > 2 && !write && !process.argv.includes('--check')) {
  throw new Error('Use --check or --write')
}

function sha256(contents: string | Buffer) {
  return createHash('sha256').update(contents).digest('hex')
}

async function source(path: string) {
  return readFile(resolve(repo, path), 'utf8')
}

function topLevelSteps(contents: string): string[] {
  const body = contents.split('---\n')[1]
  if (!body) throw new Error('Accepted flow has no YAML body')
  const matches = [...body.matchAll(/^- /gm)].map(match => match.index!)
  return matches.map((start, index) => body.slice(start, matches[index + 1] ?? body.length))
}

const coldPath = `${sourceDirectory}/cold-saved-checkout.yaml`
const memoryPath = `${sourceDirectory}/cold-stored-memory.json`
const reportPath = `${sourceDirectory}/cold-run-report.json`
const cold = await source(coldPath)
if (sha256(cold) !== acceptedFlowSha256) {
  throw new Error('Cold WALK-020 YAML differs from the accepted flow checksum')
}
const memory = JSON.parse(await source(memoryPath))
const report = JSON.parse(await source(reportPath))
if (memory.flowSha256 !== acceptedFlowSha256 || !memory.lastValidation.passed ||
    memory.buildFingerprint !== buildFingerprint || report.outcome !== 'cold-recorded' ||
    !report.passed || report.assertions !== 14 || report.identity.buildFingerprint !== buildFingerprint) {
  throw new Error('Accepted WALK-020 cold verification identity no longer matches')
}

const steps = topLevelSteps(cold)
const marker = '- assertVisible:\n    id: screen_catalog\n    text: Catalog\n'
const loginEnd = steps.indexOf(marker)
if (loginEnd !== 10 || steps.length !== 22) {
  throw new Error('Accepted WALK-020 observation sequence no longer has 11 login + 11 checkout steps')
}
const login = `appId: dev.walkcache.checkout\n---\n${steps.slice(0, loginEnd + 1).join('')}`
const continuation = steps.slice(loginEnd + 1).join('')
if ((login.match(/^\s*- assertVisible:/gm) ?? []).length !== 5 ||
    (continuation.match(/^\s*- assertVisible:/gm) ?? []).length !== 9) {
  throw new Error('Expected 5 reused and 9 new full product assertions')
}
const loginSha256 = sha256(login)
const segmentPath = `${evidenceDirectory}/segments/login-v1/android-${loginSha256}.yaml`
const continuationPath = `${evidenceDirectory}/continuation/checkout.yaml`
const composedPath = `${evidenceDirectory}/composed/sign-in-and-check-out-mochi-sticker-pack/candidate.yaml`
const contractPath = `${evidenceDirectory}/login-contract.json`
const reference = `../../segments/login-v1/android-${loginSha256}.yaml`
const composed = composeMaestroLoginFlow('dev.walkcache.checkout', reference, continuation, {
  workspaceRoot: resolve(repo, evidenceDirectory),
  flowDirectory: resolve(repo, dirname(composedPath)),
})
const contract = validateSubflowContract({
  schemaVersion: 1,
  segmentId: 'login-v1',
  identity: {
    appId: 'dev.walkcache.checkout',
    platform: 'android',
    flowSchemaVersion: 'maestro/v1',
    buildFingerprint,
  },
  inputs: [
    { name: 'username', kind: 'text', required: true },
    { name: 'password', kind: 'credential', required: true },
  ],
  outputs: [
    { name: 'authenticatedCatalog', kind: 'screen', resourceId: 'screen_catalog' },
  ],
  preconditions: [
    { kind: 'clean-app-launch' },
    { kind: 'screen-visible', resourceId: 'screen_login' },
  ],
  postconditions: [{ kind: 'screen-visible', resourceId: 'screen_catalog' }],
  stepCount: 11,
  assertionCount: 5,
  provenance: {
    kind: 'curated-evidence',
    ticket: 'WALK-020',
    runId,
    newDeviceRunRecorded: false,
    artifacts: [coldPath, memoryPath, reportPath].map(path => ({ path, sha256: sha256('') })),
  },
})
for (const artifact of contract.provenance.kind === 'curated-evidence'
  ? contract.provenance.artifacts : []) {
  artifact.sha256 = sha256(await source(artifact.path))
}
// Revalidate after filling source checksums, then serialize one deterministic contract.
const validatedContract = validateSubflowContract(contract)
const artifacts = new Map<string, string>([
  [segmentPath, login],
  [continuationPath, continuation],
  [composedPath, composed],
  [contractPath, `${JSON.stringify(validatedContract, null, 2)}\n`],
])
for (const [path, expected] of artifacts) {
  const absolute = resolve(repo, path)
  if (write) {
    await mkdir(dirname(absolute), { recursive: true })
    await writeFile(absolute, expected)
  } else if (await readFile(absolute, 'utf8') !== expected) {
    throw new Error(`${path} is stale or differs from accepted WALK-020 evidence`)
  }
}
if (composed.includes('username_input') || composed.includes('password_input') ||
    composed.includes('qa@walkcache.dev') || composed.includes('testing123') ||
    composed.includes('login_button')) {
  throw new Error('Composed YAML duplicated login commands or fixture values')
}
console.log(JSON.stringify({
  result: 'pass', mode: write ? 'write' : 'check', ticket: 'WALK-018',
  provenance: { ticket: 'WALK-020', runId, newDeviceRunRecorded: false },
  appId: 'dev.walkcache.checkout', buildFingerprint,
  sourceFlowSha256: acceptedFlowSha256, loginFlowSha256: loginSha256,
  composedFlowSha256: sha256(composed), reusedSteps: 11, newlyExploredSteps: 11,
  reusedAssertions: 5, newAssertions: 9, totalAssertions: 14,
  loginStepsDuplicated: false, artifacts: [...artifacts.keys()],
}, null, 2))
