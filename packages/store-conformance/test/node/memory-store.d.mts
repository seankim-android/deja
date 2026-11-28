import type { WalkStore } from '@deja-run/core'
import type { PortableStoreImplementation } from '@deja-run/store-conformance'

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
