// The store conformance suites on Node, through node:test. Run it with `bun run test:node`, which
// builds the packages first: this file imports their built output, never their TypeScript source.
import {
  localStoreContract,
  runConsumerFlowContractTests,
  runPortableWalkStoreContractTests,
  runSqliteDriverContractTests,
  runWalkStoreContractTests,
  runWriterLockContractTests,
  sqliteStoreContract,
} from '@deja-qa/store-conformance'
import { nodeHarness } from '@deja-qa/store-conformance/node'
import { memoryStoreContract } from './memory-store.mjs'

for (const implementation of [localStoreContract, sqliteStoreContract]) {
  runWalkStoreContractTests(implementation, nodeHarness)
  runConsumerFlowContractTests(implementation, nodeHarness)
  runWriterLockContractTests(implementation, nodeHarness)
  runPortableWalkStoreContractTests({
    name: implementation.name,
    createStore: implementation.createStore,
    closeStore: implementation.closeStore,
  }, nodeHarness)
}
runSqliteDriverContractTests(nodeHarness)
runPortableWalkStoreContractTests(memoryStoreContract, nodeHarness)
