#!/usr/bin/env bun
/* Deterministic executable fixture: implements only the curated WALK-018 YAML command subset. */
const { readFileSync } = require('node:fs')
const { dirname, resolve } = require('node:path')

const flowPath = process.argv[process.argv.length - 1]
const state = {
  screen: 'unlaunched',
  focused: null,
  usernameEntered: false,
  passwordEntered: false,
  clearStateLaunches: 0,
  steps: 0,
  reusedSteps: 0,
  assertions: 0,
  assertionObservations: [],
  finalProductAssertions: {},
}
let failed = false

const visibleByScreen = {
  login: {
    screen_login: 'WalkCache Shop',
    username_input: null,
    password_input: null,
    login_button: 'Sign in',
  },
  catalog: {
    screen_catalog: 'Catalog',
    add_to_cart: 'Add to cart',
  },
  cart: {
    screen_cart: 'Your cart',
    checkout_button: 'Checkout',
  },
  confirmation: {
    order_confirmation: 'Order confirmed',
    screen_confirmation: 'Order #WC-1001 · Mochi sticker pack',
  },
}

function scalar(value) {
  const trimmed = value.trim()
  if (trimmed.startsWith('"')) return JSON.parse(trimmed)
  return trimmed
}

function blocks(contents) {
  const result = []
  let current = null
  for (const line of contents.split(/\r?\n/)) {
    if (line.startsWith('- ')) {
      current = [line.slice(2)]
      result.push(current)
    } else if (current && /^\s+/.test(line)) {
      current.push(line.trim())
    } else if (line && !line.startsWith('#')) {
      current = null
    }
  }
  return result
}

function property(lines, name) {
  const prefix = `${name}:`
  const line = lines.find(value => value.startsWith(prefix))
  return line ? scalar(line.slice(prefix.length)) : null
}

function pass(message) {
  console.log(`${message}... COMPLETED`)
}

function fail(message, diagnostic) {
  console.log(`${message}... FAILED`)
  console.log('')
  console.log(diagnostic)
  process.exitCode = 1
  failed = true
}

function observation(value) {
  console.log(`FIXTURE_OBSERVATION ${JSON.stringify(value)}`)
}

function executeAssert(lines) {
  state.assertions += 1
  if (lines[0] !== 'assertVisible:' && lines[0].startsWith('assertVisible:')) {
    const expected = scalar(lines[0].slice('assertVisible:'.length))
    const allowed = state.screen === 'catalog'
      ? ['Mochi sticker pack', '$5.00 · 6 stickers']
      : state.screen === 'cart'
        ? ['1 × Mochi sticker pack', 'Total: $5.00']
        : []
    const message = `Assert that "${expected}" is visible`
    if (!allowed.includes(expected)) return fail(message, `Assertion is false: "${expected}" is visible`)
    pass(message)
    state.assertionObservations.push({ resourceId: null, text: expected })
    observation({ kind: 'assertVisible', resourceId: null, text: expected })
    return
  }
  const id = property(lines.slice(1), 'id')
  const declaredText = property(lines.slice(1), 'text')
  const expectedText = visibleByScreen[state.screen]?.[id]
  const known = id && Object.hasOwn(visibleByScreen[state.screen] ?? {}, id)
  const textMatches = declaredText === null || declaredText === expectedText
  const message = known && !textMatches && declaredText !== null
    ? `Assert that "${declaredText}" is visible`
    : `Assert that id: ${id} is visible`
  if (!known || !textMatches) {
    return fail(message, `Assertion is false: ${message.slice('Assert that '.length)}`)
  }
  pass(`${message}${declaredText !== null ? `, text: ${JSON.stringify(declaredText)}` : ''}`)
  state.assertionObservations.push({ resourceId: id, text: declaredText })
  observation({ kind: 'assertVisible', resourceId: id, text: declaredText })
  if (state.screen === 'confirmation' && declaredText === expectedText) {
    state.finalProductAssertions[id] = true
  }
}

function executeTap(lines) {
  const id = property(lines.slice(1), 'id') ?? scalar(lines[0].slice('tapOn:'.length))
  const message = `Tap on id: ${id}`
  if (process.env.WALKCACHE_COMPOSITION_FAIL === 'checkout-missing' && id === 'checkout_button') {
    return fail(message, 'Element not found: Id matching regex: checkout_button')
  }
  if (state.screen === 'login' && (id === 'username_input' || id === 'password_input')) {
    state.focused = id
    pass(message)
    observation({ kind: 'action', command: 'tapOn' })
    return
  }
  if (state.screen === 'login' && id === 'login_button' &&
      state.usernameEntered && state.passwordEntered) {
    state.screen = 'catalog'
    state.focused = null
    pass(message)
    observation({ kind: 'action', command: 'tapOn' })
    return
  }
  if (state.screen === 'catalog' && id === 'add_to_cart') {
    state.screen = 'cart'
    pass(message)
    observation({ kind: 'action', command: 'tapOn' })
    return
  }
  if (state.screen === 'cart' && id === 'checkout_button') {
    state.screen = 'confirmation'
    pass(message)
    observation({ kind: 'action', command: 'tapOn' })
    return
  }
  return fail(message, `Element not found: Id matching regex: ${id}`)
}

function executeFlow(path, depth) {
  if (depth > 1) return fail('Run nested flow', 'Nested runFlow depth exceeds fixture bound')
  const contents = readFileSync(path, 'utf8')
  if (!contents.startsWith('appId: dev.walkcache.checkout\n')) {
    return fail('Read fixture app identity', 'Unexpected appId')
  }
  for (const lines of blocks(contents)) {
    if (failed) return
    const command = lines[0]
    // runFlow is the composition reference, not another product action.
    if (!command.startsWith('runFlow:')) {
      state.steps += 1
      if (depth > 0) state.reusedSteps += 1
    }
    if (command === 'launchApp:') {
      const clearState = property(lines.slice(1), 'clearState')
      if (clearState !== 'true' || depth > 1) {
        return fail('Launch app "dev.walkcache.checkout" with clear state', 'App launch failed: clean launch required once')
      }
      state.screen = 'login'
      state.focused = null
      state.usernameEntered = false
      state.passwordEntered = false
      state.clearStateLaunches += 1
      pass('Launch app "dev.walkcache.checkout" with clear state')
      observation({ kind: 'launch', clearState: true })
    } else if (command === 'assertVisible:' || command.startsWith('assertVisible:')) {
      executeAssert(lines)
    } else if (command === 'tapOn:' || command.startsWith('tapOn:')) {
      executeTap(lines)
    } else if (command.startsWith('inputText:')) {
      const value = scalar(command.slice('inputText:'.length))
      if (state.screen !== 'login' || !state.focused) {
        fail(`Input text "${value}"`, 'Element not found: no focused fixture input')
      } else {
        if (state.focused === 'username_input') state.usernameEntered = true
        if (state.focused === 'password_input') state.passwordEntered = true
        pass(`Input text "${value}"`)
        observation({ kind: 'action', command: 'inputText' })
      }
    } else if (command.startsWith('runFlow:')) {
      const reference = scalar(command.slice('runFlow:'.length))
      executeFlow(resolve(dirname(path), reference), depth + 1)
    } else {
      fail(command, 'Unsupported deterministic fixture command')
    }
  }
}

executeFlow(flowPath, 0)
console.log(`FIXTURE_SUMMARY ${JSON.stringify({
  clearStateLaunches: state.clearStateLaunches,
  steps: state.steps,
  reusedSteps: state.reusedSteps,
  assertions: state.assertions,
  assertionObservations: state.assertionObservations,
  visibleResourceIds: Object.keys(visibleByScreen[state.screen] ?? {}),
  finalProductAssertionPassed: state.screen === 'confirmation' && !failed &&
    state.finalProductAssertions.order_confirmation === true &&
    state.finalProductAssertions.screen_confirmation === true,
  screen: state.screen,
})}`)
