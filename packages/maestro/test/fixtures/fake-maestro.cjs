#!/usr/bin/env bun
const { spawn } = require('node:child_process')
const { writeFileSync } = require('node:fs')
const { basename, dirname, join } = require('node:path')

const modeArgument = process.argv[2]

function stayAlive(markerPath) {
  setTimeout(() => writeFileSync(markerPath, 'process was still alive'), 900)
  setInterval(() => {}, 1_000)
}

if (modeArgument === 'grandchild' || modeArgument === 'bystander') {
  stayAlive(process.argv[3])
} else {
  const flowPath = process.argv[process.argv.length - 1]
  const mode = basename(flowPath, '.yaml')
  console.log(JSON.stringify({
    argv: process.argv.slice(2),
    androidHome: process.env.ANDROID_HOME,
    javaHome: process.env.JAVA_HOME,
    path: process.env.PATH,
  }))
  console.log('fake stdout')
  console.error('fake stderr')

  if (mode === 'nonzero') process.exitCode = 7
  if (mode === 'timeout') {
    const markerPath = join(dirname(flowPath), 'timed-out-process-survived.txt')
    spawn(process.execPath, [__filename, 'grandchild', markerPath], { stdio: 'ignore' })
    process.on('SIGTERM', () => {})
    stayAlive(join(dirname(flowPath), 'parent-survived.txt'))
  }
}
