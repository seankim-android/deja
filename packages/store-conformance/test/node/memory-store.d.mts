import type { WalkStore } from '@deja-qa/core'
import type { PortableStoreImplementation } from '@deja-qa/store-conformance'

export class MemoryWalkStore implements WalkStore {
  constructor(workspaceRoot: string)
  lookup: WalkStore['lookup']
  lookupPreviousSuccessful: WalkStore['lookupPreviousSuccessful']
  persistVerifiedCandidate: WalkStore['persistVerifiedCandidate']
  persistVerifiedRepair: WalkStore['persistVerifiedRepair']
  recordValidation: WalkStore['recordValidation']
  list: WalkStore['list']
}
export const memoryStoreContract: PortableStoreImplementation
