import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { JsonRunReporter } from '../../../apps/cli/src/reporter.ts'
import { createRedactor } from '../../../apps/cli/src/redaction.ts'
import { Deja } from '../../core/src/index.ts'
import { MaestroWalkRunner } from '../../maestro/src/index.ts'
import {
  contractIdentity,
  initialValidation,
  localStoreContract,
  runWalkStoreContractTests,
  sqliteStoreContract,
  type WalkStoreContractImplementation,
} from '../src/index.ts'

runWalkStoreContractTests(localStoreContract)
runWalkStoreContractTests(sqliteStoreContract)

const fakeMaestro = resolve(import.meta.dir, '../../maestro/test/fixtures/fake-maestro.cjs')

function runOrchestrationContract(implementation: WalkStoreContractImplementation) {
  describe(`${implementation.name}: actual orchestration conformance`, () => {
    test('one production-runner cache hit performs zero model calls and appends passing validation', async () => {
      const temporary = await mkdtemp(join(tmpdir(), 'walk019-orchestration-'))
      const workspaceRoot = join(temporary, 'workspace')
      const store = implementation.createStore(workspaceRoot)
      try {
        await mkdir(join(workspaceRoot, 'flows'), { recursive: true })
        await writeFile(
          join(workspaceRoot, 'flows/candidate.yaml'),
          'appId: dev.deja.checkout\n- assertVisible: Order confirmed\n',
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
          timeoutMs: 5000,
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
