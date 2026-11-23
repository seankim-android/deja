import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { checkStoredSubflows, collectFlowClosure, FlowClosureError } from '../src/flow-closure.ts'

async function workspace(files: Record<string, string | Buffer>) {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'deja-closure-media-')))
  for (const [path, contents] of Object.entries(files)) {
    await mkdir(join(root, path, '..'), { recursive: true })
    await writeFile(join(root, path), contents)
  }
  return { root, cleanup: () => rm(root, { recursive: true, force: true }) }
}

describe('closure with media files (DEJA-32)', () => {
  test('hashes and keeps media as bytes, with its source path', async () => {
    const picture = Buffer.from([0x89, 0x50, 0xff, 0xfe, 0x00, 0x81, 0xc3, 0x28])
    const ctx = await workspace({
      '.maestro/flows/main.yaml': '- addMedia: ../media/pic.png\n',
      '.maestro/media/pic.png': picture,
    })
    try {
      const closure = await collectFlowClosure(ctx.root, join(ctx.root, '.maestro/flows/main.yaml'))
      const media = closure.files.find(file => file.kind === 'media')!
      expect(media).toMatchObject({ path: 'media/pic.png', source: '.maestro/media/pic.png' })
      expect(media.bytes).toEqual(picture)
    } finally {
      await ctx.cleanup()
    }
  })

  test('media over 8 MiB is refused with the limit named,', async () => {
    const ctx = await workspace({
      '.maestro/flows/main.yaml': '- addMedia: ../media/big.mp4\n',
      '.maestro/media/big.mp4': Buffer.alloc(8 * 1024 * 1024 + 1),
    })
    try {
      await expect(collectFlowClosure(ctx.root, join(ctx.root, '.maestro/flows/main.yaml')))
        .rejects.toThrow(/under 8 MiB/)
    } finally {
      await ctx.cleanup()
    }
  })

  test('two Unicode spellings of one path are refused, whatever the disk does with them', async () => {
    const ctx = await workspace({
      '.maestro/flows/main.yaml': '- addMedia: ../media/café.png\n- addMedia: ../media/café.png\n',
      '.maestro/media/café.png': 'x',
    })
    try {
      await expect(collectFlowClosure(ctx.root, join(ctx.root, '.maestro/flows/main.yaml')))
        .rejects.toBeInstanceOf(FlowClosureError)
      await expect(collectFlowClosure(ctx.root, join(ctx.root, '.maestro/flows/main.yaml')))
        .rejects.toThrow(/Unicode normalization/)
    } finally {
      await ctx.cleanup()
    }
  })

  test('a stored walk from before media and retry files were kept replays only after it is remembered again', async () => {
    const ctx = await workspace({
      '.deja/flows/g/android-aa/main.yaml': '- addMedia: pic.png\n',
      '.deja/flows/g/android-aa/pic.png': 'x',
      '.deja/flows/g/android-bb/main.yaml': '- retry:\n    file: again.yaml\n',
      '.deja/flows/g/android-bb/again.yaml': '- tapOn: x\n',
      '.deja/flows/g/android-cc/main.yaml': '- runFlow: again.yaml\n',
      '.deja/flows/g/android-cc/again.yaml': '- tapOn: x\n',
    })
    try {
      expect(await checkStoredSubflows(ctx.root, '.deja/flows/g/android-aa/main.yaml', []))
        .toEqual({ status: 'legacy' })
      expect(await checkStoredSubflows(ctx.root, '.deja/flows/g/android-bb/main.yaml', []))
        .toEqual({ status: 'legacy' })
      // Any other unlisted file is still tampering, not age.
      expect(await checkStoredSubflows(ctx.root, '.deja/flows/g/android-cc/main.yaml', []))
        .toMatchObject({ status: 'invalid' })
    } finally {
      await ctx.cleanup()
    }
  })
})
