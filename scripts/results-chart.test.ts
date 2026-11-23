import { expect, test } from 'bun:test'
import { resolve } from 'node:path'

test('docs/results-chart.svg matches the saved run reports', () => {
  const run = Bun.spawnSync(['bun', resolve(import.meta.dir, 'render-results-chart.ts'), '--check'])
  expect(run.stderr.toString()).toBe('')
  expect(run.exitCode).toBe(0)
})
