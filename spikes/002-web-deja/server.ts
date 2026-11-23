#!/usr/bin/env bun
import { resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(fileURLToPath(new URL('./public', import.meta.url)))
const port = Number(process.env.DEJA_WEB_PORT ?? 4174)
const hostname = process.env.DEJA_WEB_HOST ?? '127.0.0.1'
const orderNumber = process.env.DEJA_FIXTURE_ORDER ?? 'WC-1001'
if (!/^WC-[0-9]{4}$/.test(orderNumber)) throw new Error('Fixture order must match WC-0000')

Bun.serve({
  hostname,
  port,
  async fetch(request) {
    const url = new URL(request.url)
    if (url.pathname === '/api/order') {
      return Response.json(
        { orderNumber },
        { headers: { 'cache-control': 'no-store' } },
      )
    }
    const requested = url.pathname === '/' ? '/index.html' : url.pathname
    const path = resolve(root, `.${requested}`)
    if (path !== root && !path.startsWith(`${root}${sep}`)) {
      return new Response('Not found', { status: 404 })
    }
    const file = Bun.file(path)
    if (!(await file.exists())) return new Response('Not found', { status: 404 })
    return new Response(file, {
      headers: {
        'cache-control': 'no-store',
        'content-security-policy': "default-src 'self'; script-src 'self'; style-src 'self'",
      },
    })
  },
})
console.log(`Déjà deterministic web fixture: http://${hostname}:${port}/`)
