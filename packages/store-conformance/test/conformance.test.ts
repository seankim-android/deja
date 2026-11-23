import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, realpath, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { JsonRunReporter } from '@deja-qa/cli/reporter'
import { createRedactor } from '@deja-qa/cli/redaction'
import { Deja } from '@deja-qa/core'
import { MaestroWalkRunner } from '@deja-qa/maestro'
import {
  contractIdentity,
  initialValidation,
  localStoreContract,
  runConsumerFlowContractTests,
  runWalkStoreContractTests,
  sqliteStoreContract,
  runWriterLockContractTests,
  type WalkStoreContractImplementation,
} from '../src/index.ts'

runWalkStoreContractTests(localStoreContract)
runWalkStoreContractTests(sqliteStoreContract)
runConsumerFlowContractTests(localStoreContract)
runConsumerFlowContractTests(sqliteStoreContract)
runWriterLockContractTests(localStoreContract)
runWriterLockContractTests(sqliteStoreContract)

const fakeMaestro = resolve(import.meta.dir, '../../maestro/test/fixtures/fake-maestro.cjs')

function runOrchestrationContract(implementation: WalkStoreContractImplementation) {
  describe(`${implementation.name}: actual orchestration conformance`, () => {
    test('one production-runner cache hit performs zero model calls and appends passing validation', async () => {
      const temporary = await realpath(await mkdtemp(join(tmpdir(), 'walk019-orchestration-')))
      const workspaceRoot = join(temporary, 'workspace')
      const store = implementation.createStore(workspaceRoot)
      try {
        await mkdir(join(workspaceRoot, 'flows'), { recursive: true })
        await writeFile(
          join(workspaceRoot, 'flows/candidate.yaml'),
          'appId: dev.deja.checkout\n---\n- assertVisible: Order confirmed\n',
        )
        const identity = contractIdentity()
        const persisted = await store.persistVerifiedCandidate({
          identity,
          candidatePath: 'flows/candidate.yaml',
          validation: { passed: true, at: initialValidation },
          origin: 'remember',
        })
        const runner = new MaestroWalkRunner({
          workspaceRoot,
          executable: fakeMaestro,
          device: 'deterministic-local-fixture',
          timeoutMs: 30_000,
        })
        const reporter = new JsonRunReporter(workspaceRoot, createRedactor(workspaceRoot, []))
        let explorerConstructions = 0
        let repairerConstructions = 0
        const cache = new Deja({
          store,
          runner,
          reporter,
          artifactId: () => 'walk019-conformance',
          createExplorer: () => {
            explorerConstructions += 1
            throw new Error('A cache hit must not construct an explorer or model client')
          },
          createRepairer: () => {
            repairerConstructions += 1
            throw new Error('A passing cache hit must not construct a repairer')
          },
        })
        const result = await cache.run(identity)
        expect(runner).toBeInstanceOf(MaestroWalkRunner)
        expect(result).toMatchObject({
          cacheStatus: 'hit', outcome: 'passed', passed: true, modelCalls: 0,
          assertions: 1, repairCount: 0,
          yamlPath: persisted.flowPath, yamlSha256: persisted.flowSha256,
        })
        expect(result.execution?.exitCode).toBe(0)
        expect(explorerConstructions).toBe(0)
        expect(repairerConstructions).toBe(0)
        const history = (await store.list())[0].validationHistory
        expect(history).toHaveLength(2)
        expect(history.at(-1)?.passed).toBe(true)
        expect(history.at(-1)?.flowSha256).toBe(persisted.flowSha256)
        const report = JSON.parse(
          await readFile(join(workspaceRoot, '.deja/reports/latest.json'), 'utf8'),
        )
        expect(report.modelCalls).toBe(0)
        expect(report.execution.exitCode).toBe(0)
        const output = result.execution?.artifacts.find(artifact => artifact.kind === 'output')
        expect(output).toBeDefined()
        expect(await readFile(join(workspaceRoot, output!.path), 'utf8')).toContain('fake stdout')
        console.log(`WALK019_ORCHESTRATION ${JSON.stringify({
          store: implementation.name,
          runner: 'MaestroWalkRunner (real child process, deterministic local executable fixture)',
          cacheStatus: result.cacheStatus,
          outcome: result.outcome,
          modelCalls: result.modelCalls,
          explorerConstructions,
          repairerConstructions,
          childProcessExitCode: result.execution?.exitCode,
          assertions: result.assertions,
          validationHistoryEntries: history.length,
          platformExecutionClaim: 'none; this is runner/store orchestration, not a device result',
        })}`)
      } finally {
        await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      }
    })
  })
}

runOrchestrationContract(localStoreContract)
runOrchestrationContract(sqliteStoreContract)

function runRememberSnapshotContract(implementation: WalkStoreContractImplementation) {
  describe(`${implementation.name}: remember stores what it verified`, () => {
    test('a subflow edited while verification runs is not stored as verified', async () => {
      const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja2-remember-')))
      const workspaceRoot = join(temporary, 'workspace')
      const store = implementation.createStore(workspaceRoot)
      try {
        await mkdir(join(workspaceRoot, '.maestro/flows'), { recursive: true })
        await mkdir(join(workspaceRoot, '.maestro/subflows'), { recursive: true })
        await writeFile(
          join(workspaceRoot, '.maestro/flows/main.yaml'),
          'appId: dev.deja.checkout\n---\n- runFlow: ../subflows/note.yaml\n',
        )
        const subflow = join(workspaceRoot, '.maestro/subflows/note.yaml')
        await writeFile(subflow, '- assertVisible: Verified text\n')
        const verified: string[] = []
        let ran = ''
        const deja = new Deja({
          store,
          runner: {
            run: async request => {
              ran = request.flowPath
              verified.push(await readFile(resolve(workspaceRoot, request.flowPath, '../../subflows/note.yaml'), 'utf8'))
              // An agent edits the original subflow while the run is going.
              await writeFile(subflow, '- assertVisible: Unverified text\n')
              return {
                passed: true, startedAt: initialValidation, endedAt: initialValidation, durationMs: 1,
                exitCode: 0, assertions: 1, failure: null, artifacts: [],
              }
            },
          },
          reporter: { report: async () => {} } as never,
          artifactId: () => 'snapshot-remember',
          createExplorer: () => { throw new Error('no explorer') },
          createRepairer: () => { throw new Error('no repairer') },
        })
        const result = await deja.remember(contractIdentity(), join(workspaceRoot, '.maestro/flows/main.yaml'))
        expect(result.outcome).toBe('remembered')
        expect(ran).toStartWith('.deja/attempts/snapshot-remember/candidate/')
        expect(verified).toEqual(['- assertVisible: Verified text\n'])
        const stored = await store.lookup(contractIdentity())
        if (stored.status !== 'hit') throw new Error('expected hit')
        expect(await readFile(resolve(stored.flowPath, '../../subflows/note.yaml'), 'utf8'))
          .toBe('- assertVisible: Verified text\n')
      } finally {
        await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      }
    })
  })
}

runRememberSnapshotContract(localStoreContract)
runRememberSnapshotContract(sqliteStoreContract)
