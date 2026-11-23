import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, readdir, realpath, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { emptyWalkManifest } from '@deja-qa/core'
import { ConsumerFlows, ManifestLockedError, WriterLock } from '../src/index.ts'

let root = ''

beforeEach(async () => {
  root = await realpath(await mkdtemp(join(tmpdir(), 'deja-manifest-write-')))
})

afterEach(async () => {
  await rm(root, { recursive: true, force: true })
})

describe('manifest file writes', () => {
  test('a failed write leaves no temporary file in the tree', async () => {
    // A folder where the manifest should be makes the final rename fail.
    await mkdir(join(root, 'deja.walks.json/inner'), { recursive: true })
    const consumer = new ConsumerFlows({ workspaceRoot: root, manifestPath: 'deja.walks.json' })
    await expect(consumer.writeManifest(emptyWalkManifest())).rejects.toThrow()
    expect((await readdir(root)).filter(name => name.endsWith('.tmp'))).toEqual([])
  })

  test('a symlinked manifest is refused, not replaced by a plain file', async () => {
    await writeFile(join(root, 'real.json'), '{}')
    await symlink(join(root, 'real.json'), join(root, 'deja.walks.json'))
    const consumer = new ConsumerFlows({ workspaceRoot: root, manifestPath: 'deja.walks.json' })
    await expect(consumer.writeManifest(emptyWalkManifest())).rejects.toThrow(/symbolic link/)
    expect((await readdir(root)).filter(name => name.endsWith('.tmp'))).toEqual([])
  })

  test('a busy manifest lock says so, and does not point at --store', async () => {
    const holder = new WriterLock(join(root, '.deja/manifest.lock'))
    holder.acquire()
    try {
      const consumer = new ConsumerFlows({ workspaceRoot: root, manifestPath: 'deja.walks.json' })
      const error = await consumer.withLock(async () => 1).catch(caught => caught)
      expect(error).toBeInstanceOf(ManifestLockedError)
      expect(error.message).toContain('share it')
      expect(error.message).not.toContain('--store')
    } finally {
      holder.release()
    }
  })
})
