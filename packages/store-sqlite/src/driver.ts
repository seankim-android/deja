/*
 * The one place that knows which SQLite binding is running. `bun:sqlite` on Bun, `node:sqlite` on
 * Node 22.13 and later. Each is loaded with a dynamic import behind a runtime check, so neither
 * runtime ever tries to resolve the other's module.
 *
 * What this seam makes the same on both bindings:
 * - `get()` returns `null` when there is no row (`node:sqlite` returns `undefined`).
 * - Rows are plain objects (`node:sqlite` returns objects with a null prototype).
 * - Integers come back as numbers and TEXT as strings. Blobs come back as a `Uint8Array`.
 * - Parameters are positional only.
 * - `query()` prepares a fresh statement each call (`bun:sqlite` caches by SQL text; this store
 *   never holds a statement across calls, so the difference cannot be seen).
 */

export type SqliteValue = string | number | null | Uint8Array
export type SqliteRow = Record<string, SqliteValue>

export interface SqliteStatement {
  /** The first row, or `null` when there is none. */
  get(...parameters: SqliteValue[]): SqliteRow | null
  all(...parameters: SqliteValue[]): SqliteRow[]
  run(...parameters: SqliteValue[]): void
}

export interface SqliteConnection {
  /** Run one or more statements. */
  exec(sql: string): void
  query(sql: string): SqliteStatement
  close(): void
}

export type SqliteOpenOptions = {
  /** Open an existing file read-only. Never creates the file. */
  readonly?: boolean
}

export type SqliteRuntime = 'bun' | 'node'

type Opener = (path: string, options: SqliteOpenOptions) => SqliteConnection

function plain(row: unknown): SqliteRow | null {
  return row === undefined || row === null ? null : { ...(row as SqliteRow) }
}

async function loadBun(): Promise<Opener> {
  const { Database } = await import('bun:sqlite')
  return (path, options) => {
    const database = options.readonly
      ? new Database(path, { readonly: true, readwrite: false, create: false })
      : new Database(path, { create: true, readwrite: true })
    return {
      exec: sql => { database.exec(sql) },
      query: sql => {
        const statement = database.query(sql)
        return {
          get: (...parameters) => plain(statement.get(...parameters)),
          all: (...parameters) => (statement.all(...parameters) as unknown[]).map(row => plain(row)!),
          run: (...parameters) => { statement.run(...parameters) },
        }
      },
      close: () => { database.close() },
    }
  }
}

async function loadNode(): Promise<Opener> {
  const { DatabaseSync } = await import('node:sqlite')
  return (path, options) => {
    const database = new DatabaseSync(path, options.readonly ? { readOnly: true } : {})
    return {
      exec: sql => { database.exec(sql) },
      query: sql => {
        const statement = database.prepare(sql)
        return {
          get: (...parameters) => plain(statement.get(...parameters)),
          all: (...parameters) => statement.all(...parameters).map(row => plain(row)!),
          run: (...parameters) => { statement.run(...parameters) },
        }
      },
      close: () => { database.close() },
    }
  }
}

/** Which binding this process uses. */
export const sqliteRuntime: SqliteRuntime = typeof Bun !== 'undefined' ? 'bun' : 'node'

const open: Opener = sqliteRuntime === 'bun' ? await loadBun() : await loadNode()

/** Open a SQLite database with the binding for this runtime. */
export function openSqlite(path: string, options: SqliteOpenOptions = {}): SqliteConnection {
  return open(path, options)
}
