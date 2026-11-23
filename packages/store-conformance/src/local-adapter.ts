import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import {
  LOCAL_MEMORY_PATH,
  LocalWalkStore,
} from '@deja-qa/store-local'
import type {
  MetadataTamper,
  StoreFailureInjection,
  WalkStoreContractImplementation,
} from './walk-store-contract.ts'

async function readMetadata(workspaceRoot: string) {
  return JSON.parse(await readFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH), 'utf8'))
}

async function writeMetadata(workspaceRoot: string, value: unknown) {
  await writeFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH), `${JSON.stringify(value, null, 2)}\n`)
}

function createLocalStore(workspaceRoot: string, failures: StoreFailureInjection = {}) {
  const memoryPath = resolve(workspaceRoot, LOCAL_MEMORY_PATH)
  return new LocalWalkStore({
    workspaceRoot,
    renameFile: async (from, to) => {
      if (failures.failFlowRename && to.endsWith('.yaml')) {
        throw new Error('simulated interruption before flow rename')
      }
      if (failures.failMetadataReplacement && to === memoryPath) {
        throw new Error('simulated metadata replacement failure')
      }
      await rename(from, to)
    },
  })
}

async function tamperLocalMetadata(workspaceRoot: string, kind: MetadataTamper) {
  const path = resolve(workspaceRoot, LOCAL_MEMORY_PATH)
  if (kind === 'malformed') {
    await mkdir(dirname(path), { recursive: true })
    await writeFile(path, '{not json\n')
    return
  }
  const value = await readMetadata(workspaceRoot)
  if (kind === 'unsupported-schema') value.schemaVersion = 999
  if (kind === 'invalid-entry') value.walks.push({ goalKey: 'broken', lastValidation: { passed: true } })
  if (kind === 'duplicate-identity') value.walks.push(JSON.parse(JSON.stringify(value.walks[0])))
  await writeMetadata(workspaceRoot, value)
}

async function editLocalSubflowMetadata(workspaceRoot: string, kind: 'strip' | 'empty') {
  const value = await readMetadata(workspaceRoot)
  if (kind === 'strip') delete value.walks[0].subflowFiles
  else value.walks[0].subflowFiles = []
  await writeMetadata(workspaceRoot, value)
}

async function downgradeLocalToSchema2(workspaceRoot: string) {
  const value = await readMetadata(workspaceRoot)
  value.schemaVersion = 2
  for (const walk of value.walks) delete walk.subflowFiles
  await writeMetadata(workspaceRoot, value)
}

export const localStoreContract: WalkStoreContractImplementation = {
  name: 'LocalWalkStore (filesystem + JSON)',
  metadataPath: workspaceRoot => resolve(workspaceRoot, LOCAL_MEMORY_PATH),
  createStore: createLocalStore,
  closeStore: () => {},
  snapshotMetadata: workspaceRoot => readFile(resolve(workspaceRoot, LOCAL_MEMORY_PATH)),
  tamperMetadata: tamperLocalMetadata,
  editSubflowMetadata: editLocalSubflowMetadata,
  downgradeToSchema2: downgradeLocalToSchema2,
  setFirstFlowPath: async (workspaceRoot, flowPath) => {
    const value = await readMetadata(workspaceRoot)
    value.walks[0].flowPath = flowPath
    await writeMetadata(workspaceRoot, value)
  },
}
