import { createHash } from 'node:crypto'
import { lstat, readFile, realpath, rename, rm, writeFile } from 'node:fs/promises'
import { extname, isAbsolute, relative, resolve, sep } from 'node:path'

export const CHECKOUT_COMPONENT_PATH =
  'app/src/main/java/dev/walkcache/checkout/MainActivity.java'
export const CHECKOUT_IDS_PATH = 'app/src/main/res/values/ids.xml'
export const CHECKOUT_AFFECTED_FILES = [CHECKOUT_COMPONENT_PATH, CHECKOUT_IDS_PATH] as const

export type CheckoutRepairPolicy = {
  workspaceRoot: string
  sourceRoots: string[]
  permittedFileTypes: string[]
  affectedFiles: string[]
}

export type PlannedRepairFile = {
  path: string
  absolutePath: string
  before: string
  after: string
  beforeSha256: string
  afterSha256: string
}

export type CheckoutRepairPlan = {
  policy: CheckoutRepairPolicy
  workspaceRoot: string
  files: PlannedRepairFile[]
  diff: string
}

type ExactReplacement = {
  from: string
  to: string
}

const replacements: Record<string, ExactReplacement[]> = {
  [CHECKOUT_COMPONENT_PATH]: [{
    from: '        Button checkout = button("Checkout", null);',
    to: [
      '        Integer checkoutId = Settings.Global.getInt(',
      '            getContentResolver(), CHECKOUT_SELECTOR_STALE_SETTING, 0',
      '        ) == 1 ? null : R.id.checkout_button;',
      '        Button checkout = button("Checkout", checkoutId);',
    ].join('\n'),
  }],
  [CHECKOUT_IDS_PATH]: [
    {
      from: '    <item name="screen_cart" type="id" />\n    <item name="screen_confirmation" type="id" />',
      to: '    <item name="screen_cart" type="id" />\n    <item name="checkout_button" type="id" />\n    <item name="screen_confirmation" type="id" />',
    },
    {
      from: '    <!-- checkout_button is deliberately absent for the selector-repair spike. -->',
      to: '    <!-- checkout_button was added by the verified selector-repair spike. -->',
    },
  ],
}

function inside(root: string, candidate: string) {
  return candidate !== root && candidate.startsWith(`${root}${sep}`)
}

function digest(contents: string | Buffer) {
  return createHash('sha256').update(contents).digest('hex')
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

async function rejectSymlinkComponents(root: string, candidate: string) {
  const pathRelative = relative(root, candidate)
  if (!pathRelative || pathRelative.startsWith(`..${sep}`) || isAbsolute(pathRelative)) {
    throw new Error(`Repair path escapes the canonical workspace: ${candidate}`)
  }
  let current = root
  for (const component of pathRelative.split(sep)) {
    current = resolve(current, component)
    const metadata = await lstat(current)
    if (metadata.isSymbolicLink()) {
      throw new Error(`Repair path contains a symlink: ${relative(root, current)}`)
    }
  }
}

async function canonicalWorkspace(workspaceRoot: string) {
  const root = await realpath(resolve(workspaceRoot))
  if (!(await lstat(root)).isDirectory()) throw new Error('Repair workspace must be a directory')
  return root
}

async function validatePolicy(policy: CheckoutRepairPolicy) {
  const workspaceRoot = await canonicalWorkspace(policy.workspaceRoot)
  if (policy.sourceRoots.length === 0) throw new Error('At least one canonical source root is required')
  const roots = await Promise.all(policy.sourceRoots.map(async configured => {
    if (isAbsolute(configured)) throw new Error(`Source roots must be workspace-relative: ${configured}`)
    const lexical = resolve(workspaceRoot, configured)
    if (!inside(workspaceRoot, lexical)) throw new Error(`Source root escapes workspace: ${configured}`)
    await rejectSymlinkComponents(workspaceRoot, lexical)
    const canonical = await realpath(lexical)
    if (canonical !== lexical || !inside(workspaceRoot, canonical) || !(await lstat(canonical)).isDirectory()) {
      throw new Error(`Source root is not a canonical workspace directory: ${configured}`)
    }
    return lexical
  }))
  if (policy.permittedFileTypes.length === 0 ||
      policy.permittedFileTypes.some(type => !['.java', '.xml'].includes(type))) {
    throw new Error('Permitted repair file types may contain only .java and .xml')
  }
  const expected = [...CHECKOUT_AFFECTED_FILES].sort()
  const configured = [...policy.affectedFiles].sort()
  if (configured.length !== expected.length || configured.some((path, index) => path !== expected[index])) {
    throw new Error(`Repair is restricted to exactly: ${expected.join(', ')}`)
  }
  return { workspaceRoot, roots }
}

async function canonicalRepairFile(
  policy: CheckoutRepairPolicy,
  workspaceRoot: string,
  roots: string[],
  path: string,
) {
  if (isAbsolute(path) || path.split('/').includes('..')) {
    throw new Error(`Repair file path must be a safe workspace-relative path: ${path}`)
  }
  if (!policy.permittedFileTypes.includes(extname(path))) {
    throw new Error(`Repair file type is not permitted: ${path}`)
  }
  const absolutePath = resolve(workspaceRoot, path)
  if (!inside(workspaceRoot, absolutePath) || !roots.some(root => inside(root, absolutePath))) {
    throw new Error(`Repair file is outside the configured source roots: ${path}`)
  }
  await rejectSymlinkComponents(workspaceRoot, absolutePath)
  const canonical = await realpath(absolutePath)
  if (canonical !== absolutePath || !roots.some(root => inside(root, canonical)) ||
      !(await lstat(canonical)).isFile()) {
    throw new Error(`Repair file is not a canonical regular source file: ${path}`)
  }
  return absolutePath
}

function replaceExactlyOnce(contents: string, replacement: ExactReplacement, path: string) {
  const first = contents.indexOf(replacement.from)
  const last = contents.lastIndexOf(replacement.from)
  if (first < 0 || first !== last) {
    throw new Error(`Expected exactly one validated repair match in ${path}`)
  }
  return `${contents.slice(0, first)}${replacement.to}${contents.slice(first + replacement.from.length)}`
}

function unifiedFileDiff(path: string, before: string, after: string) {
  const oldLines = before.split('\n')
  const newLines = after.split('\n')
  let prefix = 0
  while (prefix < oldLines.length && prefix < newLines.length && oldLines[prefix] === newLines[prefix]) {
    prefix += 1
  }
  let suffix = 0
  while (suffix < oldLines.length - prefix && suffix < newLines.length - prefix &&
      oldLines[oldLines.length - 1 - suffix] === newLines[newLines.length - 1 - suffix]) {
    suffix += 1
  }
  const oldStart = Math.max(0, prefix - 3)
  const newStart = Math.max(0, prefix - 3)
  const oldEnd = Math.min(oldLines.length, oldLines.length - suffix + 3)
  const newEnd = Math.min(newLines.length, newLines.length - suffix + 3)
  const lines = [
    `--- a/${path}`,
    `+++ b/${path}`,
    `@@ -${oldStart + 1},${oldEnd - oldStart} +${newStart + 1},${newEnd - newStart} @@`,
  ]
  const oldMiddleEnd = oldLines.length - suffix
  const newMiddleEnd = newLines.length - suffix
  for (let index = oldStart; index < prefix; index += 1) lines.push(` ${oldLines[index]}`)
  for (let index = prefix; index < oldMiddleEnd; index += 1) lines.push(`-${oldLines[index]}`)
  for (let index = prefix; index < newMiddleEnd; index += 1) lines.push(`+${newLines[index]}`)
  for (let index = oldMiddleEnd; index < oldEnd; index += 1) lines.push(` ${oldLines[index]}`)
  return `${lines.join('\n')}\n`
}

export function createCheckoutRepairDiff(files: Pick<PlannedRepairFile, 'path' | 'before' | 'after'>[]) {
  return files.map(file => unifiedFileDiff(file.path, file.before, file.after)).join('')
}

export async function planCheckoutSelectorRepair(
  policy: CheckoutRepairPolicy,
): Promise<CheckoutRepairPlan> {
  const { workspaceRoot, roots } = await validatePolicy(policy)
  const files: PlannedRepairFile[] = []
  for (const path of CHECKOUT_AFFECTED_FILES) {
    const absolutePath = await canonicalRepairFile(policy, workspaceRoot, roots, path)
    const before = await readFile(absolutePath, 'utf8')
    let after = before
    for (const replacement of replacements[path]) {
      after = replaceExactlyOnce(after, replacement, path)
    }
    if (after === before) throw new Error(`Validated repair would not change ${path}`)
    files.push({
      path,
      absolutePath,
      before,
      after,
      beforeSha256: digest(before),
      afterSha256: digest(after),
    })
  }
  return { policy, workspaceRoot, files, diff: createCheckoutRepairDiff(files) }
}

async function writeValidatedSource(
  plan: CheckoutRepairPlan,
  file: PlannedRepairFile,
  contents: string,
) {
  const { workspaceRoot, roots } = await validatePolicy(plan.policy)
  const canonical = await canonicalRepairFile(plan.policy, workspaceRoot, roots, file.path)
  if (canonical !== file.absolutePath) throw new Error(`Repair source moved during the attempt: ${file.path}`)
  const temporary = `${canonical}.${process.pid}.walkcache-repair.tmp`
  try {
    await writeFile(temporary, contents, { flag: 'wx' })
    const temporaryMetadata = await lstat(temporary)
    if (!temporaryMetadata.isFile() || temporaryMetadata.isSymbolicLink()) {
      throw new Error(`Repair temporary path is not a regular file: ${file.path}`)
    }
    await rename(temporary, canonical)
  } catch (error) {
    if (errorCode(error) !== 'ENOENT') await rm(temporary, { force: true }).catch(() => {})
    throw error
  }
}

export async function applyCheckoutSelectorRepair(plan: CheckoutRepairPlan) {
  for (const file of plan.files) {
    const current = await readFile(file.absolutePath, 'utf8')
    if (current !== file.before || digest(current) !== file.beforeSha256) {
      throw new Error(`Repair source changed before writing: ${file.path}`)
    }
    await writeValidatedSource(plan, file, file.after)
  }
}

export async function rollbackCheckoutSelectorRepair(plan: CheckoutRepairPlan) {
  for (const file of [...plan.files].reverse()) {
    await writeValidatedSource(plan, file, file.before)
  }
}

export async function checkoutSourceDigests(plan: CheckoutRepairPlan) {
  return Object.fromEntries(await Promise.all(plan.files.map(async file => [
    file.path,
    digest(await readFile(file.absolutePath)),
  ])))
}

export async function sourceTreeDigest(workspaceRoot: string, paths: string[]) {
  const root = await canonicalWorkspace(workspaceRoot)
  const entries = await Promise.all(paths.map(async path => {
    const absolute = resolve(root, path)
    await rejectSymlinkComponents(root, absolute)
    return [path, digest(await readFile(absolute))] as const
  }))
  return Object.fromEntries(entries)
}
