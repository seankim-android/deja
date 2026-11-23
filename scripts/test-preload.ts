import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

// Tests that start a fake Maestro must never create runtime folders under the real
// ~/.local/share/deja. Tests that need a specific DEJA_HOME set their own.
process.env.DEJA_HOME ??= mkdtempSync(join(tmpdir(), 'deja-test-home-'))
