import { createHash as hasher, randomUUID } from 'node:crypto'
import { copyFile, link, readFile, realpath, unlink } from 'node:fs/promises'
import { relative, resolve, sep } from 'node:path'
import { createHash } from 'node:crypto'
import {
  emptyWalkManifest,
  manifestWalkKey,
  parseWalkManifest,
  serializeWalkManifest,
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

  const claimed = new Set<string>()
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
    // What the plan read must be what the old store verified, file for file.
    const storedHashes = [chosen.flowSha256, ...(chosen.subflowFiles ?? []).map(file => file.sha256)].sort()
    const plannedHashes = plan.files.map(file => file.sha256).sort()
    if (storedHashes.length !== plannedHashes.length || storedHashes.some((hash, index) => hash !== plannedHashes[index])) {
      skip('copy-changed', 'the stored files changed while the migration was planned')
      continue
    }
    if (plan.secrets.length > 0) {
      skip('holds-fixture-value', 'the stored flow holds a fixture credential value, so it is not copied into your tree; remove the value, then remember it again')
      continue
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
    // An entry the reader would refuse (a goal with a path in it, say) must never be written.
    try {
      parseWalkManifest(serializeWalkManifest({ ...emptyWalkManifest(), walks: [entry] }))
    } catch (error) {
      skip('manifest-invalid', `the manifest would not read back: ${(error as Error).message.replace(/^Invalid walk manifest: /, '')}; remember it again under a goal without a path in it`)
      continue
    }
    const existing = manifest.walks.find(item => manifestWalkKey(item) === manifestWalkKey(entry))
    if (existing && (existing.flow !== entry.flow || existing.closureSha256 !== entry.closureSha256 ||
        existing.assertionsSha256 !== entry.assertionsSha256)) {
      skip('manifest-differs', `the manifest already lists "${goal}" with a different flow, closure or assertions; review it first`)
      continue
    }
    // Two goals can slug to one name ("Log in", "Log-in"). The first keeps it, in a dry run too.
    const destinations = [plan.flow, ...plan.files.map(file => file.path)]
    const taken = destinations.find(path => claimed.has(path))
    if (taken) {
      skip('slug-collision', `${taken} is already taken by another goal in this run; remember one of them under a different goal name`)
      continue
    }
    for (const path of destinations) claimed.add(path)
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
  if (converted.length > 0) {
    // A goal name that is a fixture value stops the whole run, before any file is written.
    const keys = new Set(converted.map(item => manifestWalkKey(item.entry)))
    consumer.checkManifest({
      ...manifest,
      walks: [...manifest.walks.filter(entry => !keys.has(manifestWalkKey(entry))), ...converted.map(item => item.entry)],
    })
  }
  let backupPath: string | null = null
  if (!dryRun && converted.length > 0) {
    // If a later step fails, take back what this run wrote (new files, the manifest bytes). The old
    // store is untouched until its last step, so a failed run leaves the tree as it found it.
    const manifestBefore = await consumer.manifestText()
    const written: string[] = []
    try {
      for (const item of converted) written.push(...await consumer.writeMaterialized(item.plan))
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
      // It is copied under a temporary name and linked into place, so a crash cannot leave a short file
      // under the final name for a later run to trust.
      const backup = `${context.memoryFile}.schema3.bak`
      const temporary = `${backup}.${process.pid}-${randomUUID()}.tmp`
      try {
        await copyFile(context.memoryFile, temporary)
        const copy = hasher('sha256').update(await readFile(temporary)).digest('hex')
        if (copy !== hasher('sha256').update(await readFile(context.memoryFile)).digest('hex')) {
          throw new Error('The backup copy of the old store does not match it')
        }
        await link(temporary, backup)
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error
      } finally {
        await unlink(temporary).catch(() => {})
      }
      backupPath = posix(relative(root, backup))
      const replaced = new Set(converted.flatMap(item => item.replaced))
      await context.writeWalks([
        ...walks.filter(walk => !replaced.has(walk)),
        ...converted.map(item => item.walk),
      ])
    } catch (error) {
      await consumer.removeWritten(written)
      await consumer.restoreManifestText(manifestBefore).catch(() => {})
      throw error
    }
  }
  return {
    dryRun, manifestPath: consumer.manifestPath, manifestChanged, migrated, skipped, backupPath,
  }
}
