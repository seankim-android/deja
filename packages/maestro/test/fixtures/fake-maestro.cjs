#!/usr/bin/env bun
const { spawn } = require('node:child_process')
const { readFileSync, writeFileSync } = require('node:fs')
const { basename, dirname, join } = require('node:path')

const modeArgument = process.argv[2]

function stayAlive(markerPath) {
  setTimeout(() => writeFileSync(markerPath, 'process was still alive'), 900)
  setInterval(() => {}, 1_000)
}

if (modeArgument === '--version') {
  console.log(process.env.DEJA_FAKE_MAESTRO_VERSION || '2.12.0')
} else if (modeArgument === 'grandchild' || modeArgument === 'bystander') {
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
  // DEJA_FAKE_MAESTRO_SCRIPT="crash,pass" picks a mode per call, counted in DEJA_FAKE_MAESTRO_COUNTER.
  let scripted = process.env.DEJA_FAKE_MAESTRO_MODE
  if (process.env.DEJA_FAKE_MAESTRO_SCRIPT) {
    const counter = process.env.DEJA_FAKE_MAESTRO_COUNTER
    let calls = 0
    try { calls = Number(readFileSync(counter, 'utf8')) } catch {}
    writeFileSync(counter, String(calls + 1))
    const modes = process.env.DEJA_FAKE_MAESTRO_SCRIPT.split(',')
    scripted = modes[Math.min(calls, modes.length - 1)]
  }
  console.log('fake stdout')
  console.error('fake stderr')

  if (scripted === 'selector') {
    console.log('Running on fixture')
    console.log('Launch app "dev.deja.checkout" with clear state... COMPLETED')
    console.log('Assert that id: screen_cart is visible... COMPLETED')
    console.log('Tap on id: checkout_button... FAILED')
    console.log('')
    console.log('Element not found: Id matching regex: checkout_button')
    process.exitCode = 1
  }

  if (scripted === 'product') {
    console.log('Launch app "dev.deja.checkout" with clear state... COMPLETED')
    console.log('Assert that "Order #WC-1001" is visible... FAILED')
    console.log('Assertion is false: "Order #WC-1001" is visible')
    process.exitCode = 1
  }

  if (scripted === 'crash') {
    console.log('Launch app "dev.deja.ios.checkout" with clear state... COMPLETED')
    console.log('App crashed (SIGSEGV)')
    console.log('Tap on id: checkout_button... FAILED')
    console.log('')
    console.log('Element not found: Id matching regex: checkout_button')
    process.exitCode = 1
  }

  // A crash that leaves no line in Maestro's output (an iOS crash report is the only evidence).
  if (scripted === 'quiet-crash') {
    // DEJA_FAKE_CRASH_REPORT is the .ips path to write during this run, as the system would.
    if (process.env.DEJA_FAKE_CRASH_REPORT) {
      writeFileSync(process.env.DEJA_FAKE_CRASH_REPORT, process.env.DEJA_FAKE_CRASH_REPORT_BODY ?? '')
    }
    console.log('Launch app "dev.deja.ios.checkout" with clear state... COMPLETED')
    console.log('Tap on id: checkout_button... FAILED')
    console.log('')
    console.log('Element not found: Id matching regex: checkout_button')
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
