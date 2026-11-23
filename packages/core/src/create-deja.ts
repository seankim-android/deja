import type {
  RunReporter,
  RunResult,
  SelectorRepairer,
  SemanticRetrievalOptions,
  WalkExplorer,
  WalkIdentity,
  WalkRunner,
  WalkStore,
} from './contracts.ts'
import { Deja } from './orchestration.ts'

/** Bumped on a breaking change to a plugin contract. Plugins declare a peer range on core. */
export const CONTRACT_VERSION = 1

/** The flow format the Maestro runner replays. Part of exact cache identity. */
export const DEFAULT_FLOW_SCHEMA_VERSION = 'maestro/v1'

/**
 * What core needs from a platform adapter. Core imports no runner code, so this is structural:
 * the adapters in `@deja-qa/maestro` satisfy it as they are.
 */
export interface DejaAdapter {
  readonly id: string
  readonly platform: string
  buildFingerprint(workspaceRoot: string, buildPath: string): Promise<string>
  cacheIdentity(input: { buildFingerprint: string }): {
    buildFingerprint: string
    adapter?: string
    browser?: string
  }
}

/** Passed to the lazy explorer and repairer factories. */
export type DejaFactoryContext = { identity: WalkIdentity }

export type CreateDejaOptions = {
  workspaceRoot: string
  app: {
    appId: string
    /** Build artifact, relative to `workspaceRoot`. The adapter hashes it into the identity. */
    buildPath: string
  }
  adapter: DejaAdapter
  store: WalkStore
  runner: WalkRunner
  /** Lazy: called only on a cold miss or an explicit `explore`. A cache hit never calls it. */
  explorer: (context: DejaFactoryContext) => WalkExplorer
  /** Lazy: called only for an eligible stale selector. Omit to refuse repair. */
  repairer?: (context: DejaFactoryContext) => SelectorRepairer
  /** Defaults to a reporter that does nothing. */
  reporter?: RunReporter
  semantic?: SemanticRetrievalOptions
  flowSchemaVersion?: string
  now?: () => Date
  artifactId?: (prefix: 'run' | 'remember') => string
}

export type DejaClient = {
  /** The identity `run`, `explore` and `remember` use for this goal. */
  identity(goal: string): Promise<WalkIdentity>
  run(goal: string): Promise<RunResult>
  explore(goal: string): Promise<RunResult>
  remember(goal: string, candidatePath: string): Promise<RunResult>
}

const noopReporter: RunReporter = {
  report: async () => ({ reportPath: '', latestPath: '' }),
}

export function createDeja(options: CreateDejaOptions): DejaClient {
  const identity = async (goal: string): Promise<WalkIdentity> => {
    const buildFingerprint = await options.adapter.buildFingerprint(
      options.workspaceRoot, options.app.buildPath,
    )
    return {
      goal,
      appId: options.app.appId,
      platform: options.adapter.platform,
      flowSchemaVersion: options.flowSchemaVersion ?? DEFAULT_FLOW_SCHEMA_VERSION,
      ...options.adapter.cacheIdentity({ buildFingerprint }),
    }
  }
  // One orchestrator per call, so the factories close over that call's identity.
  const orchestrator = (walkIdentity: WalkIdentity) => {
    const { explorer, repairer } = options
    return new Deja({
      store: options.store,
      runner: options.runner,
      reporter: options.reporter ?? noopReporter,
      createExplorer: () => explorer({ identity: walkIdentity }),
      ...(repairer ? { createRepairer: () => repairer({ identity: walkIdentity }) } : {}),
      ...(options.semantic ? { semantic: options.semantic } : {}),
      ...(options.now ? { now: options.now } : {}),
      ...(options.artifactId ? { artifactId: options.artifactId } : {}),
    })
  }
  return {
    identity,
    run: async goal => {
      const value = await identity(goal)
      return orchestrator(value).run(value)
    },
    explore: async goal => {
      const value = await identity(goal)
      return orchestrator(value).explore(value)
    },
    remember: async (goal, candidatePath) => {
      const value = await identity(goal)
      return orchestrator(value).remember(value, candidatePath)
    },
  }
}
