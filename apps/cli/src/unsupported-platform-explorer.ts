import type { ExplorationResult, WalkExplorer } from '../../../packages/core/src/index.ts'

/** Reviewed-YAML-only platforms never instantiate the Android explorer or its credential chain. */
export class UnsupportedPlatformExplorer implements WalkExplorer {
  private readonly platform: 'web' | 'ios'

  constructor(platform: 'web' | 'ios') {
    this.platform = platform
  }

  async explore(): Promise<ExplorationResult> {
    const web = this.platform === 'web'
    const label = web ? 'Web' : 'iOS'
    const ticket = web ? 'WALK-010' : 'WALK-011'
    const runtime = web ? 'Chromium' : 'iOS'
    return {
      passed: false,
      durationMs: 0,
      explorationDurationMs: 0,
      verificationDurationMs: null,
      mcpCalls: 0,
      modelCalls: 0,
      modelCallBasis: `${ticket} ${label} exploration is disabled; no explorer or model was invoked`,
      model: null,
      provider: null,
      assertions: null,
      flowPath: null,
      flowSha256: null,
      failure: null,
      artifacts: [],
      errors: [
        `Cold ${label} exploration is not supported in ${ticket}. Independently verified ${runtime} YAML can enter cache with remember; no model fallback was attempted.`,
      ],
    }
  }
}
