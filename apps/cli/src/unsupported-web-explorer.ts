import type { ExplorationResult, WalkExplorer } from '../../../packages/core/src/index.ts'

/** Deliberate WALK-010 boundary: no Android MCP launch/clearState policy is reused for Web. */
export class UnsupportedWebExplorer implements WalkExplorer {
  async explore(): Promise<ExplorationResult> {
    return {
      passed: false,
      durationMs: 0,
      explorationDurationMs: 0,
      verificationDurationMs: null,
      mcpCalls: 0,
      modelCalls: 0,
      modelCallBasis: 'WALK-010 Web exploration is disabled; no explorer or model was invoked',
      model: null,
      provider: null,
      assertions: null,
      flowPath: null,
      flowSha256: null,
      failure: null,
      artifacts: [],
      errors: [
        'Cold Web exploration is not supported in WALK-010. Independently verified Chromium YAML can enter cache with remember; no model fallback was attempted.',
      ],
    }
  }
}
