import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, realpath, readFile, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  CHECKOUT_AFFECTED_FILES,
  CHECKOUT_COMPONENT_PATH,
  CHECKOUT_IDS_PATH,
  planCheckoutSelectorRepair,
  type CheckoutRepairPolicy,
} from '../src/policy.ts'

const component = `final class MainActivity {
    void showCart() {
        Button checkout = button("Checkout", null);
    }
}
`
const ids = `<resources>
    <item name="screen_cart" type="id" />
    <item name="screen_confirmation" type="id" />
    <!-- checkout_button is deliberately absent for the selector-repair spike. -->
</resources>
`

async function fixture() {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-repair-policy-')))
  const root = join(temporary, 'workspace')
  await mkdir(join(root, 'app/src/main/java/dev/deja/checkout'), { recursive: true })
  await mkdir(join(root, 'app/src/main/res/values'), { recursive: true })
  await writeFile(join(root, CHECKOUT_COMPONENT_PATH), component)
  await writeFile(join(root, CHECKOUT_IDS_PATH), ids)
  const policy: CheckoutRepairPolicy = {
    workspaceRoot: root,
    sourceRoots: [
      'app/src/main/java/dev/deja/checkout',
      'app/src/main/res/values',
    ],
    permittedFileTypes: ['.java', '.xml'],
    affectedFiles: [...CHECKOUT_AFFECTED_FILES],
  }
  return { temporary, root, policy }
}

async function cleanup(temporary: string) {
  await rm(temporary, { recursive: true, force: true })
}

describe('canonical two-file repair policy', () => {
  test('produces a minimal exact diff only for the affected component and selector files', async () => {
    const context = await fixture()
    try {
      const plan = await planCheckoutSelectorRepair(context.policy)
      expect(plan.files.map(file => file.path)).toEqual([...CHECKOUT_AFFECTED_FILES])
      expect(plan.diff).toContain('Button checkout = button("Checkout", null);')
      expect(plan.diff).toContain('Button checkout = button("Checkout", checkoutId);')
      expect(plan.diff).toContain('+    <item name="checkout_button" type="id" />')
      expect(plan.diff).not.toContain('Order #')
      expect(plan.files[0].beforeSha256).not.toBe(plan.files[0].afterSha256)
      expect(plan.files[1].beforeSha256).not.toBe(plan.files[1].afterSha256)
      expect(await readFile(join(context.root, CHECKOUT_COMPONENT_PATH), 'utf8')).toBe(component)
      expect(await readFile(join(context.root, CHECKOUT_IDS_PATH), 'utf8')).toBe(ids)
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('rejects writes outside the configured affected files', async () => {
    const context = await fixture()
    try {
      context.policy.affectedFiles.push('flows/checkout.yaml')
      await expect(planCheckoutSelectorRepair(context.policy)).rejects.toThrow(
        'restricted to exactly',
      )
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('rejects unpermitted assertion/file types', async () => {
    const context = await fixture()
    try {
      context.policy.permittedFileTypes.push('.yaml')
      await expect(planCheckoutSelectorRepair(context.policy)).rejects.toThrow(
        'only .java and .xml',
      )
      context.policy.permittedFileTypes = ['.java']
      await expect(planCheckoutSelectorRepair(context.policy)).rejects.toThrow(
        'file type is not permitted',
      )
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('rejects lexical source-root escapes', async () => {
    const context = await fixture()
    try {
      context.policy.sourceRoots = ['../outside']
      await expect(planCheckoutSelectorRepair(context.policy)).rejects.toThrow(
        'Source root escapes workspace',
      )
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('rejects a symlinked source root even when its target is inside the workspace', async () => {
    if (process.platform === 'win32') return
    const context = await fixture()
    try {
      await symlink(
        join(context.root, 'app/src/main/res/values'),
        join(context.root, 'app/src/main/res/linked-values'),
      )
      context.policy.sourceRoots[1] = 'app/src/main/res/linked-values'
      await expect(planCheckoutSelectorRepair(context.policy)).rejects.toThrow('contains a symlink')
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('rejects a source-file symlink escape and leaves the outside target unchanged', async () => {
    if (process.platform === 'win32') return
    const context = await fixture()
    try {
      const outside = join(context.temporary, 'outside.java')
      await writeFile(outside, component)
      await rm(join(context.root, CHECKOUT_COMPONENT_PATH))
      await symlink(outside, join(context.root, CHECKOUT_COMPONENT_PATH))
      await expect(planCheckoutSelectorRepair(context.policy)).rejects.toThrow('contains a symlink')
      expect(await readFile(outside, 'utf8')).toBe(component)
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('fails closed if the validated stale selector text is absent or ambiguous', async () => {
    const context = await fixture()
    try {
      await writeFile(join(context.root, CHECKOUT_COMPONENT_PATH), '// already repaired\n')
      await expect(planCheckoutSelectorRepair(context.policy)).rejects.toThrow(
        'Expected exactly one validated repair match',
      )
      await writeFile(join(context.root, CHECKOUT_COMPONENT_PATH), `${component}${component}`)
      await expect(planCheckoutSelectorRepair(context.policy)).rejects.toThrow(
        'Expected exactly one validated repair match',
      )
    } finally {
      await cleanup(context.temporary)
    }
  })
})
