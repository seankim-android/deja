import type {
  RunReporter,
  RunResult,
  SelectorRepairer,
  StaleBuildPolicy,
  SemanticRetrievalOptions,
  WalkExplorer,
  WalkIdentity,
  WalkRunner,
  WalkStore,
} from './contracts.ts'
import { Deja, type DejaRememberOptions, type DejaRunOptions } from './orchestration.ts'

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

/**
 * Passed to the lazy explorer and repairer factories. This is the only way they receive the
 * call's `signal` and `timeoutMs`: core does not pass them into `explore` or `repair`, so a
 * plugin that wants to honor them must read them here.
 */
export type DejaFactoryContext = {
  identity: WalkIdentity
  signal?: AbortSignal
  timeoutMs?: number
}

export type CreateDejaOptions = {
  workspaceRoot: string
  app: {
    appId: string
    /** Build artifact, relative to `workspaceRoot`. The adapter hashes it into the identity. */
    buildPath: string
  }
  adapter: DejaAdapter
  store: WalkStore
  /**
   * The committed walk manifest, relative to `workspaceRoot` (for example `deja.walks.json`). Set
   * it, and build the store with the same path, to replay the consumer's own flow files. A store
   * that does not support a manifest is refused, so a hidden copy is never replayed by accident.
   * With a manifest: cold exploration and repair are not available yet, and `run` takes `verify`.
   */
  manifestPath?: string
  runner: WalkRunner
  /** Lazy: called only on a cold miss or an explicit `explore`. A cache hit never calls it. */
  explorer: (context: DejaFactoryContext) => WalkExplorer
  /** Lazy: called only for an eligible stale selector. Omit to refuse repair. */
  repairer?: (context: DejaFactoryContext) => SelectorRepairer
  /** Defaults to a reporter that does nothing. */
  reporter?: RunReporter
  semantic?: SemanticRetrievalOptions
  /** Replay the last verified flow on a new build instead of refusing. Off by default. */
  staleBuild?: StaleBuildPolicy
  flowSchemaVersion?: string
  now?: () => Date
  artifactId?: (prefix: 'run' | 'remember') => string
}

/** Per-call options. Both are optional and apply to the runner calls of that one call. */
export type DejaCallOptions = {
  /**
   * Checked at the start of the call, before each explorer or repairer factory call and before
   * each runner call; an aborted signal rejects with `signal.reason`. Work that has already
   * started is not interrupted: a running flow, explorer or repair can still finish and persist
   * its result. Use `timeoutMs` to bound a flow.
   */
  signal?: AbortSignal
  /** Passed to the runner as `RunnerRequest.timeoutMs`, and to the factories in the context. */
  timeoutMs?: number
}

/** Per-call options of `run`. */
export type DejaClientRunOptions = DejaCallOptions & DejaRunOptions
/** Per-call options of `remember`. */
export type DejaClientRememberOptions = DejaCallOptions & DejaRememberOptions

export type DejaClient = {
  /** The identity `run`, `explore` and `remember` use for this goal. */
  identity(goal: string): Promise<WalkIdentity>
  run(goal: string, callOptions?: DejaClientRunOptions): Promise<RunResult>
  explore(goal: string, callOptions?: DejaCallOptions): Promise<RunResult>
  /** `candidatePath` is a file path the runner and store can read (a Maestro YAML on disk today). */
  remember(goal: string, candidatePath: string, callOptions?: DejaClientRememberOptions): Promise<RunResult>
}

const noopReporter: RunReporter = {
  report: async () => ({ reportPath: '', latestPath: '' }),
}

export function createDeja(options: CreateDejaOptions): DejaClient {
  if (options.store.manifestPath !== undefined && options.repairer) {
    // Repair works on a stored copy; with a manifest the flow is the consumer's own file (DEJA-6).
    throw new TypeError('Repair is not supported with a manifest yet; omit the repairer')
  }
  if (options.manifestPath !== undefined && options.store.manifestPath !== options.manifestPath) {
    throw new TypeError(
      `manifestPath ${JSON.stringify(options.manifestPath)} needs a store built for the same manifest ` +
      `(store.manifestPath is ${JSON.stringify(options.store.manifestPath ?? null)}); ` +
      'refusing to replay a stored copy instead of the consumer\'s file',
    )
  }
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
  const orchestrator = (walkIdentity: WalkIdentity, callOptions: DejaCallOptions = {}) => {
    const { explorer, repairer } = options
    const { signal, timeoutMs } = callOptions
    const context: DejaFactoryContext = {
      identity: walkIdentity,
      ...(signal ? { signal } : {}),
      ...(timeoutMs !== undefined ? { timeoutMs } : {}),
    }
    const runner: WalkRunner = signal === undefined && timeoutMs === undefined
      ? options.runner
      : {
        run: request => {
          signal?.throwIfAborted()
          return options.runner.run({
            ...request,
            ...(timeoutMs !== undefined ? { timeoutMs } : {}),
          })
        },
      }
    return new Deja({
      store: options.store,
      runner,
      reporter: options.reporter ?? noopReporter,
      createExplorer: () => {
        signal?.throwIfAborted()
        return explorer(context)
      },
      ...(repairer ? {
        createRepairer: () => {
          signal?.throwIfAborted()
          return repairer(context)
        },
      } : {}),
      ...(options.semantic ? { semantic: options.semantic } : {}),
      ...(options.staleBuild ? { staleBuild: options.staleBuild } : {}),
      ...(options.now ? { now: options.now } : {}),
      ...(options.artifactId ? { artifactId: options.artifactId } : {}),
    })
  }
  return {
    identity,
    run: async (goal, callOptions) => {
      callOptions?.signal?.throwIfAborted()
      const value = await identity(goal)
      return orchestrator(value, callOptions).run(value, {
        ...(callOptions?.verify ? { verify: true } : {}),
      })
    },
    explore: async (goal, callOptions) => {
      callOptions?.signal?.throwIfAborted()
      const value = await identity(goal)
      return orchestrator(value, callOptions).explore(value)
    },
    remember: async (goal, candidatePath, callOptions) => {
      callOptions?.signal?.throwIfAborted()
      const value = await identity(goal)
      return orchestrator(value, callOptions).remember(value, candidatePath, {
        ...(callOptions?.acceptAssertionChange ? { acceptAssertionChange: true } : {}),
      })
    },
  }
}
