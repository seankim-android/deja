import { copyFile, constants, readFile, realpath } from 'node:fs/promises'
import { relative, resolve, sep } from 'node:path'
import { createHash } from 'node:crypto'
import {
  manifestWalkKey,
  type ManifestWalk,
  type MigratedWalk,
  type MigrateOptions,
  type MigrationReport,
  type SkippedWalk,
  type StoredWalk,
} from '@deja-qa/core'
import { ConsumerFlows, ConsumerFlowError, ManifestChangedError, type MaterializePlan } from './consumer-flow.ts'
import { checkStoredSubflows, FlowClosureError } from './flow-closure.ts'

function posix(path: string) {
  return path.split(sep).join('/')
}

function logicalKey(walk: StoredWalk, adapter: string | undefined) {
  return JSON.stringify([
    walk.goalKey, walk.appId, walk.platform, adapter ?? null, walk.browser ?? null,
  ])
}

export type MigrationContext = {
  /** The canonical workspace root. */
  root: string
  consumer: ConsumerFlows
  walks: StoredWalk[]
  options: MigrateOptions
  /** The store's own file (canonical path), copied beside itself before it is rewritten. */
  memoryFile: string
  /** Replace the store's walks. Called last, once everything else is written. */
  writeWalks(walks: StoredWalk[]): Promise<void> | void
}

/**
 * Turn the schema 3 walks of one store into the consumer's own files plus manifest entries. Call
 * under the store's writer lock and `consumer.withLock` (a dry run needs neither).
 *
 * Per goal it takes the newest passing walk whose stored copy is intact, writes that closure once
 * into the flows folder (never over a different file), adds the manifest entry, and rewrites the
 * store with history for that walk. The order is files, manifest, backup, store: a crash before the
 * last step leaves the store as schema 3, and a second run finds the files and entry already there.
 * Walks that cannot move stay in the store and are reported; nothing is guessed.
 */
export async function runMigration(context: MigrationContext): Promise<MigrationReport> {
  const { root, consumer, walks, options } = context
  const dryRun = options.dryRun === true
  const manifest = await consumer.load()
  const groups = new Map<string, StoredWalk[]>()
  for (const walk of walks) {
    if (walk.closureSha256 !== undefined) continue
    const key = logicalKey(walk, walk.adapter ?? options.adapter)
    groups.set(key, [...(groups.get(key) ?? []), walk])
  }

  const migrated: MigratedWalk[] = []
  const skipped: SkippedWalk[] = []
  const converted: { walk: StoredWalk; plan: MaterializePlan; entry: ManifestWalk; replaced: StoredWalk[]; existing: ManifestWalk | undefined }[] = []

  for (const group of groups.values()) {
    const passing = group.filter(walk => walk.lastValidation.passed)
      .sort((left, right) => right.lastValidation.at.localeCompare(left.lastValidation.at))
    const goal = group[0].goal
    const skip = (reason: SkippedWalk['reason'], detail: string) => skipped.push({ goal, reason, detail })
    const chosen = passing[0]
    if (!chosen) {
      skip('not-passing', 'no walk for this goal last passed, so there is nothing verified to move')
      continue
    }
    const adapter = chosen.adapter ?? options.adapter
    if (!adapter) {
      skip('adapter-unknown', 'the walk predates recorded adapters; pass the adapter id to name it in the manifest')
      continue
    }
    let source: string
    try {
      source = await realpath(resolve(root, chosen.flowPath))
      const flowRoot = await realpath(resolve(root, '.deja/flows'))
      if (!source.startsWith(`${flowRoot}${sep}`)) throw new Error('outside .deja/flows')
    } catch (error) {
      skip('copy-missing', `the stored copy cannot be read: ${(error as Error).message}`)
      continue
    }
    if (createHash('sha256').update(await readFile(source)).digest('hex') !== chosen.flowSha256) {
      skip('copy-changed', 'the stored copy no longer matches the checksum it was verified with')
      continue
    }
    const check = await checkStoredSubflows(root, chosen.flowPath, chosen.subflowFiles, chosen.legacyClosure === true)
    if (check.status === 'legacy') {
      skip('subflows-not-stored', 'the flow runs files that the old store never kept; re-remember it from your own files')
      continue
    }
    if (check.status === 'missing') {
      skip('copy-missing', `the stored copy lost ${check.path}`)
      continue
    }
    if (check.status === 'invalid') {
      skip('copy-changed', check.message)
      continue
    }
    let plan: MaterializePlan
    try {
      plan = await consumer.planMaterialize(source, chosen.goalKey)
    } catch (error) {
      if (error instanceof FlowClosureError || error instanceof ConsumerFlowError) {
        skip('closure-error', error.message)
        continue
      }
      throw error
    }
    if (plan.conflicts.length > 0) {
      skip('would-overwrite', `${plan.conflicts.join(', ')} already exist with different bytes; move or remove them, then run again`)
      continue
    }
    const entry: ManifestWalk = {
      goal: chosen.goal, appId: chosen.appId, platform: chosen.platform, adapter,
      ...(chosen.browser !== undefined ? { browser: chosen.browser } : {}),
      flow: plan.flow, closureSha256: plan.closureSha256, assertionsSha256: plan.assertionsSha256,
    }
    const existing = manifest.walks.find(item => manifestWalkKey(item) === manifestWalkKey(entry))
    if (existing && (existing.flow !== entry.flow || existing.closureSha256 !== entry.closureSha256 ||
        existing.assertionsSha256 !== entry.assertionsSha256)) {
      skip('manifest-differs', `the manifest already lists "${goal}" with a different flow, closure or assertions; review it first`)
      continue
    }
    // History of the walk that moves: only entries made with the bytes that moved.
    const history = chosen.validationHistory.filter(item => item.flowSha256 === chosen.flowSha256)
    const { subflowFiles: _files, legacyClosure: _legacy, ...rest } = chosen
    const walk: StoredWalk = {
      ...rest,
      adapter,
      flowPath: plan.flow,
      flowSha256: plan.mainSha256,
      closureSha256: plan.closureSha256,
      validationHistory: history,
    }
    converted.push({ walk, plan, entry, replaced: group, existing })
    migrated.push({
      goal, platform: chosen.platform, flow: plan.flow, closureSha256: plan.closureSha256,
      assertionsSha256: plan.assertionsSha256,
      filesWritten: plan.files.filter(file => !file.exists).map(file => file.path),
      filesUnchanged: plan.files.filter(file => file.exists).map(file => file.path),
      historyEntries: history.length,
      olderWalksDropped: group.length - 1,
    })
  }

  const manifestChanged = converted.some(item => !item.existing)
  let backupPath: string | null = null
  if (!dryRun && converted.length > 0) {
    for (const item of converted) await consumer.writeMaterialized(item.plan)
    if (manifestChanged) {
      const current = await consumer.load()
      for (const item of converted) {
        const now = current.walks.find(entry => manifestWalkKey(entry) === manifestWalkKey(item.entry))
        if (JSON.stringify(now ?? null) !== JSON.stringify(item.existing ?? null)) throw new ManifestChangedError()
      }
      const keys = new Set(converted.map(item => manifestWalkKey(item.entry)))
      await consumer.writeManifest({
        ...current,
        walks: [...current.walks.filter(entry => !keys.has(manifestWalkKey(entry))), ...converted.map(item => item.entry)],
      })
    }
    // The old store stays beside the new one. The first backup wins, so a re-run never replaces it.
    const backup = `${context.memoryFile}.schema3.bak`
    try {
      await copyFile(context.memoryFile, backup, constants.COPYFILE_EXCL)
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error
    }
    backupPath = posix(relative(root, backup))
    const replaced = new Set(converted.flatMap(item => item.replaced))
    await context.writeWalks([
      ...walks.filter(walk => !replaced.has(walk)),
      ...converted.map(item => item.walk),
    ])
  }
  return {
    dryRun, manifestPath: consumer.manifestPath, manifestChanged, migrated, skipped, backupPath,
  }
}
