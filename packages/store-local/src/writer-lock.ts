import { randomUUID } from 'node:crypto'
import {
  linkSync,
  mkdirSync,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs'
import { spawnSync } from 'node:child_process'
import { hostname } from 'node:os'
import { dirname } from 'node:path'

export type LockHolder = {
  version: 1
  pid: number
  /** Start time of the holder, to tell it from a later process that reused the pid. */
  startedAt: string | null
  host: string
  /** Stable machine id, so a changed hostname on the same machine does not hide a dead holder. */
  machineId?: string | null
  token: string
  acquiredAt: string
}

/** Another writer holds this store. Give each concurrent run its own store (`storeId`). */
export class StoreLockedError extends Error {
  readonly code: string = 'STORE_LOCKED'
  readonly lockPath: string
  readonly holder: LockHolder | null

  constructor(lockPath: string, holder: LockHolder | null, reason?: string) {
    const who = holder
      ? `process ${holder.pid} on ${holder.host} (since ${holder.acquiredAt})`
      : reason ?? 'an unreadable lock file'
    super(
      `Store is locked by ${who}: ${lockPath}. Only one writer may use a store. ` +
      'Give each concurrent run its own store (storeId, or --store <id> on the CLI). ' +
      'If no run is active, delete the lock file.',
    )
    this.name = 'StoreLockedError'
    this.lockPath = lockPath
    this.holder = holder
  }
}

/** Two writers tried to take over the same stale lock at once. The loser may simply try again. */
export class StoreLockContentionError extends StoreLockedError {
  override readonly code = 'STORE_LOCK_CONTENTION'

  constructor(lockPath: string, holder: LockHolder | null, reason: string) {
    super(lockPath, holder, reason)
    this.name = 'StoreLockContentionError'
    this.message = `Store lock contention (${reason}): ${lockPath}. Another run is taking over a ` +
      'stale lock. Try again. If it keeps failing, delete the .lock and .break files next to it.'
  }
}

/** The filesystem cannot make the lock file (no hard links, read-only, permissions). */
export class StoreLockUnavailableError extends Error {
  readonly code = 'STORE_LOCK_UNAVAILABLE'
  constructor(lockPath: string, cause: unknown) {
    super(
      `Cannot create the writer lock ${lockPath}: ${(cause as Error)?.message ?? String(cause)}. ` +
      'The writer lock needs a local filesystem that supports hard links.',
      { cause },
    )
    this.name = 'StoreLockUnavailableError'
  }
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

/**
 * Start time of a process, or null when it cannot be read. Linux reads ticks since boot from
 * /proc/<pid>/stat field 22: `ps lstart` there is derived from the boot time, which moves when the
 * clock steps. Elsewhere `ps lstart` is stable. BusyBox `ps` has no `lstart`: null, so a reused pid
 * is not detected (the lock is then held until deleted).
 */
function processStart(pid: number): string | null {
  if (process.platform === 'linux') {
    try {
      const stat = readFileSync(`/proc/${pid}/stat`, 'utf8')
      // The command name sits in parentheses and may contain spaces; fields 3.. follow the last ")".
      const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ')
      const ticks = fields[22 - 3]
      if (ticks) return `proc:${ticks}`
    } catch { /* fall through to ps */ }
  }
  try {
    const result = spawnSync('ps', ['-o', 'lstart=', '-p', String(pid)], {
      encoding: 'utf8',
      env: { ...process.env, LC_ALL: 'C' },
      timeout: 5000,
    })
    const text = result.status === 0 ? result.stdout.trim() : ''
    return text || null
  } catch {
    return null
  }
}

let ownStart: string | null | undefined
function ownProcessStart() {
  if (ownStart === undefined) ownStart = processStart(process.pid)
  return ownStart
}

let ownMachine: string | null | undefined
function machineId(): string | null {
  if (ownMachine !== undefined) return ownMachine
  ownMachine = null
  try {
    if (process.platform === 'linux') {
      ownMachine = readFileSync('/etc/machine-id', 'utf8').trim() || null
    } else if (process.platform === 'darwin') {
      const result = spawnSync('ioreg', ['-rd1', '-c', 'IOPlatformExpertDevice'], { encoding: 'utf8', timeout: 5000 })
      ownMachine = /"IOPlatformUUID" = "([^"]+)"/.exec(result.stdout ?? '')?.[1] ?? null
    }
  } catch { /* no machine id */ }
  return ownMachine
}

function sameMachine(holder: LockHolder) {
  if (holder.host === hostname()) return true
  const mine = machineId()
  return mine !== null && holder.machineId === mine
}

/**
 * True only when the holder is provably gone: same machine, and its pid is dead or now belongs to a
 * process that started at a different time. Anything unclear counts as alive (fail closed).
 */
function isStale(holder: LockHolder) {
  if (!sameMachine(holder)) return false
  try {
    process.kill(holder.pid, 0)
  } catch (error) {
    return errorCode(error) === 'ESRCH'
  }
  if (!holder.startedAt) return false
  const current = holder.pid === process.pid ? ownProcessStart() : processStart(holder.pid)
  return current !== null && current !== holder.startedAt
}

function parseHolder(text: string): LockHolder | null {
  try {
    const value = JSON.parse(text) as Record<string, unknown>
    if (value.version !== 1 || !Number.isInteger(value.pid) || typeof value.host !== 'string' ||
        typeof value.token !== 'string' || typeof value.acquiredAt !== 'string' ||
        !(value.startedAt === null || typeof value.startedAt === 'string')) return null
    return value as LockHolder
  } catch {
    return null
  }
}

function readHolder(path: string): { holder: LockHolder | null; exists: boolean } {
  try {
    return { holder: parseHolder(readFileSync(path, 'utf8')), exists: true }
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return { holder: null, exists: false }
    throw error
  }
}

/** An unreadable breaker file this old is the leftover of a crash between create and write. */
const UNREADABLE_BREAKER_STALE_MS = 10_000

function newHolder(): LockHolder {
  return {
    version: 1,
    pid: process.pid,
    startedAt: ownProcessStart(),
    host: hostname(),
    machineId: machineId(),
    token: randomUUID(),
    acquiredAt: new Date().toISOString(),
  }
}

function sleep(milliseconds: number) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds)
}

/** Publish a complete file at `target` only if nothing is there. Returns false when it exists. */
function publish(target: string, holder: LockHolder, unavailableFor: string) {
  const temporary = `${target}.${process.pid}.${holder.token}.tmp`
  writeFileSync(temporary, `${JSON.stringify(holder)}\n`, { flag: 'wx' })
  try {
    linkSync(temporary, target)
    return true
  } catch (error) {
    if (errorCode(error) === 'EEXIST') return false
    throw new StoreLockUnavailableError(unavailableFor, error)
  } finally {
    try { unlinkSync(temporary) } catch { /* already gone */ }
  }
}

/**
 * Move `path` aside to a name only we know, then read what we moved. Nothing is deleted by path:
 * if the moved file is not the one we judged dead (someone replaced it first), it is put back with
 * an exclusive link, or we report contention. Returns true when we removed the file we meant to.
 */
function removeObserved(path: string, observed: LockHolder | null, hooks?: LockTestHooks) {
  hooks?.beforeMoveAside?.(path)
  const aside = `${path}.aside.${process.pid}.${randomUUID()}`
  try {
    renameSync(path, aside)
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return true // already gone: someone else removed it
    throw error
  }
  const moved = readHolder(aside)
  const sameFile = observed ? moved.holder?.token === observed.token : moved.holder === null
  if (sameFile) {
    try { unlinkSync(aside) } catch { /* already gone */ }
    return true
  }
  // We moved a different file than the one we judged. Put it back if the slot is still free.
  try {
    linkSync(aside, path)
  } catch { /* someone else created a new one; the moved file stays aside for inspection */ }
  try { unlinkSync(aside) } catch { /* already gone */ }
  return false
}

export type LockTestHooks = {
  /** Test seam: runs after a stale breaker or lock was judged dead, before it is moved aside. */
  beforeMoveAside?: (path: string) => void
}

const held = new Set<WriterLock>()
let exitHookInstalled = false

/**
 * One writer per store. The lock file is made complete by a hard link, so another process never
 * sees a half-written one. It is released by `release()`, at process exit, or on SIGINT/SIGTERM;
 * after a crash the next writer finds the holder gone and takes over.
 */
export class WriterLock {
  readonly path: string
  private token: string | null = null
  private readonly hooks: LockTestHooks | undefined

  constructor(path: string, hooks?: LockTestHooks) {
    this.path = path
    this.hooks = hooks
  }

  get isHeld() {
    return this.token !== null
  }

  acquire() {
    if (this.token) {
      // Still ours? A lock taken from us by a faulty takeover must stop the write, not be ignored.
      const current = readHolder(this.path)
      if (current.holder?.token === this.token) return
      this.token = null
      held.delete(this)
      throw new StoreLockedError(this.path, current.holder, 'the lock was taken from this process')
    }
    mkdirSync(dirname(this.path), { recursive: true })
    const holder = newHolder()
    for (let attempt = 0; attempt < 4; attempt += 1) {
      if (publish(this.path, holder, this.path)) {
        this.token = holder.token
        held.add(this)
        this.installExitHook()
        return
      }
      const current = readHolder(this.path)
      if (!current.exists) continue
      if (!current.holder) throw new StoreLockedError(this.path, null)
      if (!isStale(current.holder)) throw new StoreLockedError(this.path, current.holder)
      this.breakStale(current.holder)
    }
    throw new StoreLockContentionError(this.path, null, 'the lock kept changing')
  }

  /** Remove a dead holder's lock. A `.break` file keeps two takers from racing each other. */
  private breakStale(stale: LockHolder) {
    const breaker = `${this.path}.break`
    const mine = newHolder()
    let acquired = false
    for (let attempt = 0; attempt < 5 && !acquired; attempt += 1) {
      if (publish(breaker, mine, this.path)) {
        acquired = true
        break
      }
      const other = readHolder(breaker)
      if (!other.exists) continue
      const unreadableAndOld = !other.holder && (() => {
        try { return Date.now() - statSync(breaker).mtimeMs > UNREADABLE_BREAKER_STALE_MS } catch { return false }
      })()
      if ((other.holder && isStale(other.holder)) || unreadableAndOld) {
        removeObserved(breaker, other.holder, this.hooks)
        continue
      }
      sleep(40) // a live taker is working: give it a moment
    }
    if (!acquired) {
      throw new StoreLockContentionError(this.path, stale, 'another process is taking over the stale lock')
    }
    try {
      const again = readHolder(this.path)
      if (again.holder && again.holder.token === stale.token && isStale(again.holder)) {
        removeObserved(this.path, again.holder, this.hooks)
      }
    } finally {
      // Only remove a breaker that still carries our token.
      if (readHolder(breaker).holder?.token === mine.token) {
        try { unlinkSync(breaker) } catch { /* already gone */ }
      }
    }
  }

  release() {
    if (!this.token) return
    const token = this.token
    this.token = null
    held.delete(this)
    try {
      const current = readHolder(this.path)
      if (current.holder?.token === token) unlinkSync(this.path)
    } catch { /* nothing to release */ }
  }

  private installExitHook() {
    if (exitHookInstalled) return
    exitHookInstalled = true
    const releaseAll = () => { for (const lock of [...held]) lock.release() }
    process.on('exit', releaseAll)
    // Ctrl-C and `kill` are how runs normally stop. Release, then let the signal take its usual
    // course. A host that handles the signal itself keeps control: it should call close().
    for (const signal of ['SIGINT', 'SIGTERM'] as const) {
      if (process.listenerCount(signal) > 0) continue
      const handler = () => {
        releaseAll()
        process.removeListener(signal, handler)
        process.kill(process.pid, signal)
      }
      process.on(signal, handler)
    }
  }
}

const STORE_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/

export class InvalidStoreIdError extends Error {
  constructor(id: string) {
    super(
      `Invalid store id ${JSON.stringify(id)}: use 1 to 128 letters, digits, dot, dash or underscore, ` +
      'starting with a letter or digit.',
    )
    this.name = 'InvalidStoreIdError'
  }
}

/**
 * The metadata path for a store. No id is the original single store. An id gives its own folder, so
 * each device (for example a simulator UDID) has its own history and its own writer lock.
 */
export function storeMetadataPath(defaultPath: string, storeId: string | undefined) {
  if (storeId === undefined) return defaultPath
  if (!STORE_ID.test(storeId)) throw new InvalidStoreIdError(storeId)
  const slash = defaultPath.lastIndexOf('/')
  return `${defaultPath.slice(0, slash)}/stores/${storeId}${defaultPath.slice(slash)}`
}
