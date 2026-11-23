#!/usr/bin/env bun
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import {
  compareColdWarm,
  renderColdWarmMarkdown,
  type MetricSample,
} from '@deja-qa/metrics'

const root = process.cwd()
const inputDirectory = resolve(root, '.deja/metrics/walk005/samples')
const outputDirectory = resolve(root, 'evidence')
const samples = (await readdir(inputDirectory)).filter(filename => filename.endsWith('.json'))
if (samples.length === 0) throw new Error(`No measured samples in ${inputDirectory}`)
samples.sort()
const parsed: MetricSample[] = []
for (const filename of samples) {
  parsed.push(JSON.parse(await readFile(resolve(inputDirectory, filename), 'utf8')) as MetricSample)
}
const comparison = compareColdWarm(parsed)
await mkdir(outputDirectory, { recursive: true })
for (const sample of parsed) {
  await writeFile(
    resolve(outputDirectory, `walk005-sample-${sample.sampleId}.json`),
    `${JSON.stringify(sample, null, 2)}\n`,
  )
}
await writeFile(
  resolve(outputDirectory, 'walk005-comparison.json'),
  `${JSON.stringify(comparison, null, 2)}\n`,
)
await writeFile(
  resolve(outputDirectory, 'walk005-comparison.md'),
  renderColdWarmMarkdown(comparison),
)
console.log(`Wrote ${parsed.length} raw sample reports and cold/warm JSON/Markdown comparisons to evidence/`)
