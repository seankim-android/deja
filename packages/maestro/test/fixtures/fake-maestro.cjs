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

  if (process.env.DEJA_FAKE_MAESTRO_MODE === 'selector') {
    console.log('Running on fixture')
    console.log('Launch app "dev.deja.checkout" with clear state... COMPLETED')
    console.log('Assert that id: screen_cart is visible... COMPLETED')
    console.log('Tap on id: checkout_button... FAILED')
    console.log('')
    console.log('Element not found: Id matching regex: checkout_button')
    process.exitCode = 1
  }

  if (process.env.DEJA_FAKE_MAESTRO_MODE === 'product') {
    console.log('Launch app "dev.deja.checkout" with clear state... COMPLETED')
    console.log('Assert that "Order #WC-1001" is visible... FAILED')
    console.log('Assertion is false: "Order #WC-1001" is visible')
    process.exitCode = 1
  }

  if (mode === 'nonzero') process.exitCode = 7

  // Like Maestro, write a JUnit report only when asked (`--format=JUNIT --output=<file>`).
  const output = process.argv.find(argument => argument.startsWith('--output='))
  if (output && process.argv.includes('--format=JUNIT')) {
    const failed = Boolean(process.exitCode)
    // Already XML text, inserted verbatim, so tests can use entity forms.
    const extra = process.env.DEJA_FAKE_JUNIT_TEXT ?? ''
    writeFileSync(output.slice('--output='.length), `<?xml version='1.0' encoding='UTF-8'?>
<testsuites>
    <testsuite name="Test Suite" device="fake" tests="1" failures="${failed ? 1 : 0}" time="0.1">
        <testcase id="${mode}" name="${mode} ${extra}" classname="${mode}" time="0.1" status="${failed ? 'ERROR' : 'SUCCESS'}">${failed ? `<failure message="fake maestro failure ${extra}">fake maestro failure ${extra}</failure>` : ''}</testcase>
    </testsuite>
</testsuites>
`)
  }
  if (mode === 'timeout') {
    const markerPath = join(dirname(flowPath), 'timed-out-process-survived.txt')
    spawn(process.execPath, [__filename, 'grandchild', markerPath], { stdio: 'ignore' })
    process.on('SIGTERM', () => {})
    stayAlive(join(dirname(flowPath), 'parent-survived.txt'))
  }
}
