import type { IncomingMessage, ServerResponse } from 'node:http'
import type { Plugin } from 'vite'
import { viewerHandler } from './local-server.ts'

type Next = (error?: unknown) => void

function middleware(workspaceRoot: string) {
  const handler = viewerHandler(workspaceRoot)
  return (request: IncomingMessage, response: ServerResponse, next: Next) => {
    if (!(request.url ?? '').startsWith('/api/walkcache/')) {
      next()
      return
    }
    void handler(request, response).catch(() => {
      if (!response.headersSent) {
        response.writeHead(500, { 'Content-Type': 'application/json; charset=utf-8' })
      }
      response.end(JSON.stringify({ state: 'unavailable', message: 'Local inspection is unavailable.' }))
    })
  }
}

/** Vite is the optional viewer host only. No CLI or lifecycle module depends on this plugin. */
export function walkCacheViewer(workspaceRoot = process.cwd()): Plugin {
  return {
    name: 'walkcache-local-run-viewer',
    configureServer(server) {
      server.middlewares.use(middleware(workspaceRoot))
    },
    configurePreviewServer(server) {
      server.middlewares.use(middleware(workspaceRoot))
    },
  }
}
