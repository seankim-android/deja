import { randomUUID } from 'node:crypto'
import {
  closeSync,
  linkSync,
  mkdirSync,
  openSync,
  readFileSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs'
import { spawnSync } from 'node:child_process'
import { hostname } from 'node:os'
import { dirname } from 'node:path'

export type LockHolder = {
  version: 1
  pid: number
  /** `ps` start time of the holder, to tell it from a later process that reused the pid. */
  startedAt: string | null
  host: string
  token: string
  acquiredAt: string
}

/** Another writer holds this store. Give each concurrent run its own store (`storeId`). */
export class StoreLockedError extends Error {
  readonly code = 'STORE_LOCKED'
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

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

/** Start time of a process as `ps` prints it, or null when it cannot be read. */
function processStart(pid: number): string | null {
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

/**
 * True only when the holder is provably gone: same host, and its pid is dead or now belongs to a
 * process that started at a different time. Anything unclear counts as alive (fail closed).
 */
function isStale(holder: LockHolder) {
  if (holder.host !== hostname()) return false
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

const held = new Set<WriterLock>()
let exitHookInstalled = false

/**
 * One writer per store. The lock file is made complete by a hard link, so another process never
 * sees a half-written one. It is released by `release()` or at process exit; after a crash the next
 * writer finds the holder gone and takes over.
 */
export class WriterLock {
  readonly path: string
  private token: string | null = null

  constructor(path: string) {
    this.path = path
  }

  get isHeld() {
    return this.token !== null
  }

  acquire() {
    if (this.token) return
    mkdirSync(dirname(this.path), { recursive: true })
    const token = randomUUID()
    const holder: LockHolder = {
      version: 1,
      pid: process.pid,
      startedAt: ownProcessStart(),
      host: hostname(),
      token,
      acquiredAt: new Date().toISOString(),
    }
    const temporary = `${this.path}.${process.pid}.${token}.tmp`
    writeFileSync(temporary, `${JSON.stringify(holder)}\n`, { flag: 'wx' })
    try {
      for (let attempt = 0; attempt < 4; attempt += 1) {
        try {
          linkSync(temporary, this.path)
          this.token = token
          held.add(this)
          this.installExitHook()
          return
        } catch (error) {
          if (errorCode(error) !== 'EEXIST') throw error
        }
        const current = readHolder(this.path)
        if (!current.exists) continue
        if (!current.holder) throw new StoreLockedError(this.path, null)
        if (!isStale(current.holder)) throw new StoreLockedError(this.path, current.holder)
        this.breakStale(current.holder)
      }
      throw new StoreLockedError(this.path, null, 'a lock that keeps changing')
    } finally {
      try { unlinkSync(temporary) } catch { /* already gone */ }
    }
  }

  /** Remove a dead holder's lock. A short `.break` file keeps two takers from racing each other. */
  private breakStale(stale: LockHolder) {
    const breaker = `${this.path}.break`
    let fd: number
    try {
      fd = openSync(breaker, 'wx')
    } catch (error) {
      if (errorCode(error) !== 'EEXIST') throw error
      const other = readHolder(breaker)
      // A breaker whose owner died is itself stale. Otherwise someone else is taking over: refuse.
      if (other.holder && isStale(other.holder)) {
        try { unlinkSync(breaker) } catch { /* lost the race to another taker */ }
        return
      }
      if (!other.exists) return
      throw new StoreLockedError(this.path, stale, 'another process is taking over a stale lock')
    }
    try {
      writeFileSync(fd, `${JSON.stringify({
        version: 1, pid: process.pid, startedAt: ownProcessStart(), host: hostname(),
        token: randomUUID(), acquiredAt: new Date().toISOString(),
      })}\n`)
      closeSync(fd)
      const again = readHolder(this.path)
      if (again.holder && again.holder.token === stale.token && isStale(again.holder)) {
        unlinkSync(this.path)
      }
    } finally {
      try { unlinkSync(breaker) } catch { /* already gone */ }
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
    process.on('exit', () => {
      for (const lock of [...held]) lock.release()
    })
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
