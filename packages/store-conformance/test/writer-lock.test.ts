import { describe, expect, test } from 'bun:test'
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync, utimesSync, writeFileSync } from 'node:fs'
import { mkdtemp, realpath, rm } from 'node:fs/promises'
import { hostname, tmpdir } from 'node:os'
import { join } from 'node:path'
import { StoreLockContentionError, StoreLockedError, StoreLockUnavailableError, WriterLock } from '@deja-run/store-local'

async function lockDirectory() {
  const directory = await realpath(await mkdtemp(join(tmpdir(), 'deja4-wl-')))
  return { directory, path: join(directory, 'store.lock') }
}

/** A pid that is certainly gone. */
function deadPid() {
  return spawnSync(process.execPath, ['-e', '']).pid!
}

function holderJson(overrides: Record<string, unknown> = {}) {
  return `${JSON.stringify({
    version: 1, pid: deadPid(), startedAt: null, host: hostname(), token: 'dead-holder',
    acquiredAt: '2026-11-22T00:00:00.000Z', ...overrides,
  })}\n`
}

const tokenOf = (path: string) => JSON.parse(readFileSync(path, 'utf8')).token

describe('WriterLock takeover', () => {
  test('a taker that read a dead breaker cannot delete the live breaker that replaced it', async () => {
    const { directory, path } = await lockDirectory()
    try {
      writeFileSync(path, holderJson())
      writeFileSync(`${path}.break`, holderJson({ token: 'dead-breaker' }))
      const taker = new WriterLock(path, {
        beforeMoveAside: moved => {
          if (moved !== `${path}.break`) return
          // Between the taker's read and its move: the dead breaker is replaced by another taker's
          // live one, which is where the review's check-then-unlink lost it.
          rmSyncIf(`${path}.break`)
          writeFileSync(`${path}.break`, `${JSON.stringify({
            version: 1, pid: process.pid, startedAt: null, host: hostname(), token: 'live-breaker',
            acquiredAt: '2026-11-23T00:00:00.000Z',
          })}\n`)
        },
      })
      // The live breaker never goes away in this test: the taker gives up with contention.
      expect(() => taker.acquire()).toThrow(StoreLockContentionError)
      expect(tokenOf(`${path}.break`)).toBe('live-breaker')
      expect(tokenOf(path)).toBe('dead-holder')
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('a taker that read a dead breaker, while another taker finishes the whole takeover, ends with one holder', async () => {
    const { directory, path } = await lockDirectory()
    try {
      writeFileSync(path, holderJson())
      writeFileSync(`${path}.break`, holderJson({ token: 'dead-breaker' }))
      const other = new WriterLock(path)
      let ran = false
      const slow = new WriterLock(path, {
        beforeMoveAside: moved => {
          if (ran || moved !== `${path}.break`) return
          ran = true
          other.acquire() // B: removes the dead breaker, breaks the stale lock, takes the lock
        },
      })
      expect(() => slow.acquire()).toThrow(StoreLockedError)
      expect(other.isHeld).toBe(true)
      expect(slow.isHeld).toBe(false)
      expect(tokenOf(path)).not.toBe('dead-holder')
      other.acquire() // still ours
      other.release()
      expect(existsSync(path)).toBe(false)
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('a holder whose lock was replaced notices before it writes again', async () => {
    const { directory, path } = await lockDirectory()
    try {
      const lock = new WriterLock(path)
      lock.acquire()
      writeFileSync(path, holderJson({ pid: process.pid, token: 'someone-else' }))
      expect(() => lock.acquire()).toThrow(StoreLockedError)
      expect(lock.isHeld).toBe(false)
      expect(tokenOf(path)).toBe('someone-else') // never deletes a lock it does not own
      lock.release()
      expect(tokenOf(path)).toBe('someone-else')
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('an empty breaker left by a crash goes stale after a timeout; a fresh one is contention', async () => {
    const { directory, path } = await lockDirectory()
    try {
      writeFileSync(path, holderJson())
      writeFileSync(`${path}.break`, '')
      const fresh = new WriterLock(path)
      const error = (() => { try { fresh.acquire() } catch (caught) { return caught as StoreLockedError } })()
      expect(error).toBeInstanceOf(StoreLockContentionError)
      expect(error!.code).toBe('STORE_LOCK_CONTENTION')
      const old = new Date(Date.now() - 60_000)
      utimesSync(`${path}.break`, old, old)
      const lock = new WriterLock(path)
      lock.acquire()
      expect(lock.isHeld).toBe(true)
      expect(existsSync(`${path}.break`)).toBe(false)
      lock.release()
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('a dead holder on the same machine with a renamed host is recovered; a different machine is not', async () => {
    const { directory, path } = await lockDirectory()
    try {
      const probe = new WriterLock(path)
      probe.acquire()
      const mine = JSON.parse(readFileSync(path, 'utf8'))
      probe.release()
      writeFileSync(path, holderJson({ host: 'old-network-name.invalid', machineId: 'some-other-machine' }))
      expect(() => new WriterLock(path).acquire()).toThrow(StoreLockedError)
      if (mine.machineId) {
        writeFileSync(path, holderJson({ host: 'old-network-name.invalid', machineId: mine.machineId }))
        const lock = new WriterLock(path)
        lock.acquire()
        expect(lock.isHeld).toBe(true)
        lock.release()
      }
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('a filesystem that cannot make the lock gives a clear error, not a raw one', async () => {
    const { directory } = await lockDirectory()
    try {
      // A lock path inside a path component that is a file cannot be created.
      writeFileSync(join(directory, 'file'), '')
      const lock = new WriterLock(join(directory, 'file', 'sub', 'x.lock'))
      expect(() => lock.acquire()).toThrow()
      // A directory where the link target is not allowed: read-only parent.
      const unavailable = new StoreLockUnavailableError('x', new Error('EPERM'))
      expect(unavailable.code).toBe('STORE_LOCK_UNAVAILABLE')
      expect(unavailable.message).toContain('hard links')
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test.skipIf(process.platform !== 'linux')('on Linux the start time comes from /proc, not ps', async () => {
    const { directory, path } = await lockDirectory()
    try {
      const lock = new WriterLock(path)
      lock.acquire()
      expect(JSON.parse(readFileSync(path, 'utf8')).startedAt).toMatch(/^proc:\d+$/)
      lock.release()
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })
})

describe('signals and a host that also listens', () => {
  test('a host listener added after the first write owns shutdown: no release, no re-raise', async () => {
    const { directory, path } = await lockDirectory()
    const lock = new WriterLock(path)
    const received: string[] = []
    const host = () => { received.push('host') }
    try {
      lock.acquire()
      process.on('SIGTERM', host) // after the first write, the usual order
      // Run only our handler; the real signal would also reach the host listener.
      const ours = process.listeners('SIGTERM').find(listener => listener !== host && String(listener).includes('releaseAll'))!
      expect(ours).toBeDefined()
      ours.call(process, 'SIGTERM')
      expect(lock.isHeld).toBe(true)
      expect(existsSync(path)).toBe(true)
      expect(process.listeners('SIGTERM')).toContain(ours) // not removed, nothing re-raised
    } finally {
      process.removeListener('SIGTERM', host)
      lock.release()
      await rm(directory, { recursive: true, force: true })
    }
  })
})

function rmSyncIf(path: string) {
  try { require('node:fs').unlinkSync(path) } catch { /* gone */ }
}
