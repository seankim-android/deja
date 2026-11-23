import { constants } from 'node:fs'
import { open, lstat, readdir } from 'node:fs/promises'
import { homedir } from 'node:os'
import { join } from 'node:path'

/**
 * Explicit app-crash evidence. Nothing else counts: an element that is missing, an app that "may
 * have crashed or been closed" (Maestro's AppNotRunning text), or a launch error is not a crash.
 */
export type MaestroCrashSignal = {
  source: 'maestro-output' | 'crash-report'
  /** Crash report file name only, never a path and never its contents. */
  reportFile?: string
  /** Process name from the report's header, validated to a short plain token. */
  process?: string
}

/**
 * Whole output lines taken from the Maestro 2.12.0 jars (strings in maestro-client and
 * maestro-ios-driver): `AppTermination.summary` ("App crashed (SIGSEGV)" or "App crashed (signal N)"),
 * `AppCrashReport.message` ("App crashed"), AndroidDriver's "App crashed (unknown cause)", and
 * IOSDriver's "<label>: <bundle> (pid N) was terminated by <signal>." Anchored to the whole line, so
 * a flow or app that merely prints similar words inside another message does not match.
 *
 * Known limit: The 2.12.0 sources show no prefix or format on these lines that a flow cannot produce, so a flow
 * whose own output (for example an `echo`) is exactly one of these lines can spoof the label. The
 * label is advisory and never repair-eligible, so the cost is a wrong label, not a repair.
 */
const CRASH_LINES = [
  /^App crashed(?: \((?:SIG[A-Z]+|signal \d+|unknown cause)\))?$/,
  /^[^\r\n]{0,200}\(pid \d+\) was terminated by (?:SIG[A-Z]+|signal \d+)\.$/,
]

export function detectCrashInOutput(output: string): MaestroCrashSignal | null {
  for (const line of output.split(/\r?\n/)) {
    const text = line.trim()
    if (CRASH_LINES.some(pattern => pattern.test(text))) return { source: 'maestro-output' }
  }
  return null
}

/** Where macOS writes crash reports for simulator apps. A seam: tests and hosts pass their own. */
export function defaultCrashReportDirectory(home = homedir()) {
  return join(home, 'Library', 'Logs', 'DiagnosticReports')
}

export type FindCrashReportOptions = {
  directory: string
  appId: string
  /** Inclusive run window, as epoch milliseconds. Only a report written inside it counts. */
  sinceMs: number
  untilMs: number
}

// Maestro's IPSParser reads these keys. The header and the first part of the body are enough.
const HEADER_BYTES = 32 * 1024
/** Real reports run to hundreds of KB; anything far beyond is not a crash report. */
const MAX_REPORT_BYTES = 8 * 1024 * 1024
const PROCESS_NAME = /^[A-Za-z0-9 ._+-]{1,64}$/

function escapeRegex(text: string) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

async function readHead(path: string) {
  // O_NOFOLLOW closes the window between lstat and open: a link swapped in is refused.
  const handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW)
  try {
    const buffer = Buffer.alloc(HEADER_BYTES)
    const { bytesRead } = await handle.read(buffer, 0, HEADER_BYTES, 0)
    return buffer.subarray(0, bytesRead).toString('utf8')
  } finally {
    await handle.close()
  }
}

/**
 * The newest `.ips` crash report for this app ID written during the run window, or null. A crash
 * report can hold personal data, so only a bounded head is read to match the app ID, and only the
 * file name and the process name leave this function.
 */
export async function findCrashReport(options: FindCrashReportOptions): Promise<MaestroCrashSignal | null> {
  let names: string[]
  try {
    names = (await readdir(options.directory)).filter(name => name.endsWith('.ips'))
  } catch {
    return null
  }
  const appId = new RegExp(
    `"(?:bundleID|CFBundleIdentifier)"\\s*:\\s*"${escapeRegex(options.appId)}"`,
  )
  const inWindow: { name: string; mtimeMs: number }[] = []
  for (const name of names) {
    try {
      // lstat, and a regular file only: a symlink could point anywhere, so it is never read.
      const info = await lstat(join(options.directory, name))
      if (info.isFile() && !info.isSymbolicLink() && info.size <= MAX_REPORT_BYTES && info.mtimeMs >= options.sinceMs && info.mtimeMs <= options.untilMs) {
        inWindow.push({ name, mtimeMs: info.mtimeMs })
      }
    } catch {
      // A report that vanished or cannot be read is not evidence.
    }
  }
  inWindow.sort((left, right) => right.mtimeMs - left.mtimeMs)
  for (const { name } of inWindow) {
    let head: string
    try {
      head = await readHead(join(options.directory, name))
    } catch {
      continue
    }
    if (!appId.test(head)) continue
    const process = /"(?:procName|app_name)"\s*:\s*"([^"]*)"/.exec(head)?.[1]
    return {
      source: 'crash-report',
      reportFile: name,
      ...(process !== undefined && PROCESS_NAME.test(process) ? { process } : {}),
    }
  }
  return null
}
