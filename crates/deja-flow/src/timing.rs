//! Phase and per-step latency telemetry primitives for flow execution and benchmarking.

/// Timing record for a single executed flow step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepTiming {
    pub step_index: usize,
    pub command: String,
    pub duration_ms: u64,
    pub retries: u32,
    pub settled_early: bool,
}

/// End-to-end phase timing breakdown for a single flow run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunTimingBreakdown {
    pub build_fingerprint_ms: u64,
    pub driver_attach_ms: u64,
    pub warmup_ms: u64,
    pub flow_exec_ms: u64,
    pub store_ms: u64,
    pub total_ms: u64,
    pub steps: Vec<StepTiming>,
}

impl RunTimingBreakdown {
    /// Sum of all recorded per-step durations in milliseconds.
    pub fn total_step_ms(&self) -> u64 {
        self.steps
            .iter()
            .fold(0_u64, |acc, step| acc.saturating_add(step.duration_ms))
    }

    /// Computes latency summary statistics over recorded step durations.
    pub fn step_latency_stats(&self) -> Option<LatencyStats> {
        let samples: Vec<f64> = self.steps.iter().map(|s| s.duration_ms as f64).collect();
        LatencyStats::from_samples(&samples)
    }
}

/// Summary distribution statistics (`min`, `p50`, `p95`, `max`) in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyStats {
    pub count: usize,
    pub min_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

impl LatencyStats {
    /// Computes deterministic nearest-rank percentile statistics over finite samples.
    /// Returns `None` when `samples` is empty or contains any non-finite value.
    pub fn from_samples(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() || samples.iter().any(|v| !v.is_finite()) {
            return None;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        let n = sorted.len();
        Some(Self {
            count: n,
            min_ms: sorted[0],
            p50_ms: nearest_rank(&sorted, 50),
            p95_ms: nearest_rank(&sorted, 95),
            max_ms: sorted[n - 1],
        })
    }
}

fn nearest_rank(sorted: &[f64], percentile: usize) -> f64 {
    let n = sorted.len();
    // ceil(percentile / 100 * n) in 1-based indexing, clamped to [1, n].
    let rank = (percentile * n).div_ceil(100).clamp(1, n);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::{LatencyStats, RunTimingBreakdown, StepTiming};

    #[test]
    fn latency_stats_rejects_empty_and_non_finite() {
        assert!(LatencyStats::from_samples(&[]).is_none());
        assert!(LatencyStats::from_samples(&[10.0, f64::NAN]).is_none());
        assert!(LatencyStats::from_samples(&[f64::INFINITY]).is_none());
    }

    #[test]
    fn latency_stats_computes_single_and_multi_sample_percentiles() {
        let single = LatencyStats::from_samples(&[42.5]).expect("single sample");
        assert_eq!(single.count, 1);
        assert_eq!(single.min_ms, 42.5);
        assert_eq!(single.p50_ms, 42.5);
        assert_eq!(single.p95_ms, 42.5);
        assert_eq!(single.max_ms, 42.5);

        let samples: Vec<f64> = (1..=20).map(|i| i as f64 * 10.0).rev().collect();
        let stats = LatencyStats::from_samples(&samples).expect("20 samples");
        assert_eq!(stats.count, 20);
        assert_eq!(stats.min_ms, 10.0);
        assert_eq!(stats.p50_ms, 100.0);
        assert_eq!(stats.p95_ms, 190.0);
        assert_eq!(stats.max_ms, 200.0);
    }

    #[test]
    fn run_timing_breakdown_aggregates_steps() {
        let breakdown = RunTimingBreakdown {
            build_fingerprint_ms: 4,
            driver_attach_ms: 0,
            warmup_ms: 120,
            flow_exec_ms: 850,
            store_ms: 2,
            total_ms: 976,
            steps: vec![
                StepTiming {
                    step_index: 0,
                    command: "launchApp".to_string(),
                    duration_ms: 400,
                    retries: 0,
                    settled_early: false,
                },
                StepTiming {
                    step_index: 1,
                    command: "tapOn".to_string(),
                    duration_ms: 150,
                    retries: 1,
                    settled_early: true,
                },
                StepTiming {
                    step_index: 2,
                    command: "assertVisible".to_string(),
                    duration_ms: 300,
                    retries: 0,
                    settled_early: true,
                },
            ],
        };
        assert_eq!(breakdown.total_step_ms(), 850);
        let stats = breakdown.step_latency_stats().expect("step stats");
        assert_eq!(stats.min_ms, 150.0);
        assert_eq!(stats.p50_ms, 300.0);
        assert_eq!(stats.max_ms, 400.0);
    }
}
