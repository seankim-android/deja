// A separate process that opens one store, takes the writer lock with a real write, and holds it.
// Usage: bun writer-process.ts <local|sqlite> <workspace> <storeId or -> [exit-now]
// Prints ACQUIRED or LOCKED:<code>, then (if it holds the lock) waits for a line on stdin.
import { localStoreContract, sqliteStoreContract, contractIdentity } from '../../src/index.ts'

const [kind, workspaceRoot, storeArgument, mode] = process.argv.slice(2)
const implementation = kind === 'sqlite' ? sqliteStoreContract : localStoreContract
const store = implementation.createStore(workspaceRoot, {}, storeArgument === '-' ? undefined : storeArgument)
try {
  await store.persistVerifiedCandidate({
    identity: contractIdentity({ goal: `race ${process.pid}` }),
    candidatePath: 'flows/candidate.yaml',
    validation: { passed: true, at: '2026-11-22T00:00:00.000Z' },
    origin: 'remember',
  })
} catch (error) {
  console.log(`LOCKED:${(error as { code?: string }).code ?? 'other'}`)
  process.exit(3)
}
console.log('ACQUIRED')
if (mode !== 'exit-now') {
  await new Promise<void>(resolveLine => process.stdin.once('data', () => resolveLine()))
}
await implementation.closeStore(store)
process.exit(0)
