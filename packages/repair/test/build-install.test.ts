import { describe, expect, test } from 'bun:test'
import { existsSync } from 'node:fs'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { runCheckoutBuildAndInstall } from '../src/build.ts'

async function fixture(gradleExitCode: number) {
  const temporary = await mkdtemp(join(tmpdir(), 'walkcache-build-install-'))
  const gradle = join(temporary, 'fake-gradlew')
  const adb = join(temporary, 'fake-adb')
  const installMarker = join(temporary, 'install-args.txt')
  await writeFile(gradle, `#!/bin/sh\necho BUILD SUCCESSFUL\nexit ${gradleExitCode}\n`, { mode: 0o755 })
  await writeFile(adb, `#!/bin/sh\nprintf '%s\\n' "$@" > "${installMarker}"\necho Performing Streamed Install\necho Success\n`, { mode: 0o755 })
  return { temporary, gradle, adb, installMarker }
}

describe('bounded Gradle build and device install', () => {
  test('installs the exact rebuilt APK on the configured device within the shared lifecycle', async () => {
    if (process.platform === 'win32') return
    const context = await fixture(0)
    try {
      const apk = join(context.temporary, 'app-debug.apk')
      await writeFile(apk, 'fixture APK')
      const result = await runCheckoutBuildAndInstall({
        workspaceRoot: context.temporary,
        gradleExecutable: context.gradle,
        adbExecutable: context.adb,
        device: 'emulator-test',
        apkPath: apk,
        timeoutMs: 2000,
      })
      expect(result.passed).toBe(true)
      expect(result.exitCode).toBe(0)
      expect(result.installExitCode).toBe(0)
      expect(result.output).toContain('BUILD SUCCESSFUL')
      expect(result.output).toContain('Performing Streamed Install\nSuccess')
      expect(result.installCommand).toEqual([
        context.adb, '-s', 'emulator-test', 'install', '-r', '-g', apk,
      ])
      expect((await readFile(context.installMarker, 'utf8')).trim().split('\n')).toEqual([
        '-s', 'emulator-test', 'install', '-r', '-g', apk,
      ])
    } finally {
      await rm(context.temporary, { recursive: true, force: true })
    }
  })

  test('never installs a failed build', async () => {
    if (process.platform === 'win32') return
    const context = await fixture(1)
    try {
      const result = await runCheckoutBuildAndInstall({
        workspaceRoot: context.temporary,
        gradleExecutable: context.gradle,
        adbExecutable: context.adb,
        device: 'emulator-test',
        apkPath: join(context.temporary, 'app-debug.apk'),
        timeoutMs: 2000,
      })
      expect(result.passed).toBe(false)
      expect(result.exitCode).toBe(1)
      expect(result.installCommand).toBeUndefined()
      expect(existsSync(context.installMarker)).toBe(false)
    } finally {
      await rm(context.temporary, { recursive: true, force: true })
    }
  })
})
