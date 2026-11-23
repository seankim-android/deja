import { Database } from 'bun:sqlite'
import { readFile, rename } from 'node:fs/promises'
import { resolve } from 'node:path'
import type { WalkStore } from '@deja-qa/core'
import {
  SQLITE_MEMORY_PATH,
  SQLiteWalkStore,
} from '@deja-qa/store-sqlite'
import type {
  MetadataTamper,
  StoreFailureInjection,
  WalkStoreContractImplementation,
} from './walk-store-contract.ts'

function databasePath(workspaceRoot: string) {
  return resolve(workspaceRoot, SQLITE_MEMORY_PATH)
}

function createSQLiteStore(workspaceRoot: string, failures: StoreFailureInjection = {}) {
  return new SQLiteWalkStore({
    workspaceRoot,
    renameFile: async (from, to) => {
      if (failures.failFlowRename) throw new Error('simulated interruption before flow rename')
      await rename(from, to)
    },
    ...(failures.failMetadataReplacement ? {
      replaceMetadata: () => { throw new Error('simulated metadata replacement failure') },
    } : {}),
  })
}

function editDatabase(workspaceRoot: string, edit: (database: Database) => void) {
  const database = new Database(databasePath(workspaceRoot))
  try {
    database.exec('PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 1000; BEGIN IMMEDIATE;')
    try {
      edit(database)
      database.exec('COMMIT')
    } catch (error) {
      database.exec('ROLLBACK')
      throw error
    }
  } finally {
    database.close()
  }
}

function cloneFirstWalk(database: Database, invalid = false) {
  const original = database.query(
    'SELECT id, goal_key, flow_sha256 FROM walks ORDER BY id LIMIT 1',
  ).get() as { id: number; goal_key: string; flow_sha256: string }
  const inserted = database.query(`
    INSERT INTO walks (
      goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
      adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin, exploration_json, has_files
    )
    SELECT goal, ?, app_id, platform, flow_schema_version, build_fingerprint,
      adapter, browser, flow_path, ?, last_passed, last_at, origin, exploration_json, has_files
    FROM walks ORDER BY id LIMIT 1 RETURNING id
  `).get(
    invalid ? 'broken' : original.goal_key,
    invalid ? 'bad' : original.flow_sha256,
  ) as { id: number }
  database.query(`
    INSERT INTO validation_history (walk_id, position, passed, at, flow_sha256)
    SELECT ?, position, passed, at, flow_sha256 FROM validation_history WHERE walk_id = ?
  `).run(inserted.id, original.id)
}

async function tamperSQLiteMetadata(workspaceRoot: string, kind: MetadataTamper) {
  editDatabase(workspaceRoot, database => {
    if (kind === 'malformed') {
      database.query('UPDATE walks SET exploration_json = ? WHERE id = (SELECT MIN(id) FROM walks)').run('{not json')
    } else if (kind === 'unsupported-schema') {
      database.exec('PRAGMA user_version = 999')
    } else if (kind === 'invalid-entry') {
      cloneFirstWalk(database, true)
    } else if (kind === 'duplicate-identity') {
      cloneFirstWalk(database)
    }
  })
}

function editSQLiteSubflowMetadata(workspaceRoot: string, kind: 'strip' | 'empty') {
  editDatabase(workspaceRoot, database => {
    database.query('DELETE FROM walk_files WHERE walk_id = (SELECT id FROM walks ORDER BY id LIMIT 1)').run()
    if (kind === 'strip') database.query('UPDATE walks SET has_files = 0 WHERE id = (SELECT id FROM walks ORDER BY id LIMIT 1)').run()
  })
  return Promise.resolve()
}

function downgradeSQLiteToSchema2(workspaceRoot: string) {
  editDatabase(workspaceRoot, database => {
    database.exec('DELETE FROM walk_files; UPDATE walks SET has_files = 0; PRAGMA user_version = 2')
  })
  return Promise.resolve()
}

export const sqliteStoreContract: WalkStoreContractImplementation = {
  name: 'SQLiteWalkStore (Bun SQLite + YAML)',
  metadataPath: databasePath,
  createStore: createSQLiteStore,
  closeStore: (store: WalkStore) => (store as SQLiteWalkStore).close(),
  snapshotMetadata: workspaceRoot => readFile(databasePath(workspaceRoot)),
  tamperMetadata: tamperSQLiteMetadata,
  editSubflowMetadata: editSQLiteSubflowMetadata,
  downgradeToSchema2: downgradeSQLiteToSchema2,
  setFirstFlowPath: async (workspaceRoot, flowPath) => {
    editDatabase(workspaceRoot, database => {
      database.query('UPDATE walks SET flow_path = ? WHERE id = (SELECT MIN(id) FROM walks)').run(flowPath)
    })
  },
}
