import { mkdirSync, symlinkSync, statSync, lstatSync } from 'node:fs'
import { homedir } from 'node:os'
import { join, resolve } from 'node:path'
import { MAESTRO_ENGINE_PIN, dejaHome, pinnedExecutablePath, type MaestroEnginePin } from './engine.ts'

/**
 * Maestro keeps its state under the JVM's `user.home` (`~/.maestro/sessions`, `deps`, `analytics.json`,
 * `tests`, ...). `XDG_STATE_HOME` moves only some of it. So every Maestro process Déjà starts runs
 * with its own `user.home`, and Déjà never writes to the real `~/.maestro` that the user's own
 * Maestro and other sessions share. `HOME` is left alone: `xcrun`, `simctl` and the Xcode
 * toolchain keep finding the real user's simulators.
 */
export const RUNTIME_HOME_OVERRIDE_DIRECTORY = 'override'

export type MaestroChildEnvOptions = {
  /** Base environment. Defaults to `process.env`. `DEJA_HOME` is read from it. */
  env?: NodeJS.ProcessEnv
  /** The Déjà home. Defaults to `DEJA_HOME` or `~/.local/share/deja`. */
  home?: string
  pin?: MaestroEnginePin
  /** The real user home, only used to link crash reports. Defaults to `os.homedir()`. */
  realHome?: string
  /** Do not create the runtime folder (tests that only read the value). */
  skipCreate?: boolean
}

/** `$DEJA_HOME/runtime/<version>` for the pinned engine, `$DEJA_HOME/runtime/override` for any other executable. */
export function maestroRuntimeHome(executable: string, options: MaestroChildEnvOptions = {}) {
  const env = options.env ?? process.env
  const home = options.home ?? dejaHome(env)
  const pin = options.pin ?? MAESTRO_ENGINE_PIN
  const pinned = resolve(executable) === resolve(pinnedExecutablePath(home, pin))
  return join(home, 'runtime', pinned ? pin.version : RUNTIME_HOME_OVERRIDE_DIRECTORY)
}

function appendJvmOption(existing: string | undefined, option: string) {
  const current = existing?.trim()
  return current ? `${current} ${option}` : option
}

/** The JVM option parser splits on spaces, so a path with spaces is double-quoted. */
function userHomeOption(runtimeHome: string) {
  return /\s/.test(runtimeHome) ? `"-Duser.home=${runtimeHome}"` : `-Duser.home=${runtimeHome}`
}

/**
 * Maestro's iOS crash detection reads `<user.home>/Library/Logs/DiagnosticReports`. A read-only
 * link to the real folder keeps it working inside the runtime home. Best effort.
 */
function linkCrashReports(runtimeHome: string, realHome: string) {
  try {
    const target = join(realHome, 'Library', 'Logs', 'DiagnosticReports')
    if (!statSync(target, { throwIfNoEntry: false })?.isDirectory()) return
    const logs = join(runtimeHome, 'Library', 'Logs')
    mkdirSync(logs, { recursive: true, mode: 0o700 })
    const link = join(logs, 'DiagnosticReports')
    if (lstatSync(link, { throwIfNoEntry: false })) return
    symlinkSync(target, link)
  } catch {
    // Crash reports are extra evidence. Isolation must not fail because of them.
  }
}

/**
 * The environment for every Maestro child process: analytics and notices off, and a private
 * `user.home`. An existing `JAVA_TOOL_OPTIONS` is kept; ours goes last so it wins.
 */
export function maestroChildEnv(executable: string, options: MaestroChildEnvOptions = {}): NodeJS.ProcessEnv {
  const env = options.env ?? process.env
  const runtimeHome = maestroRuntimeHome(executable, options)
  if (!options.skipCreate) {
    try {
      mkdirSync(runtimeHome, { recursive: true, mode: 0o700 })
      linkCrashReports(runtimeHome, options.realHome ?? homedir())
    } catch {
      // Maestro creates the folder itself. If it cannot, Maestro reports the real error.
    }
  }
  return {
    ...env,
    MAESTRO_CLI_NO_ANALYTICS: '1',
    MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED: 'true',
    JAVA_TOOL_OPTIONS: appendJvmOption(env.JAVA_TOOL_OPTIONS, userHomeOption(runtimeHome)),
  }
}
