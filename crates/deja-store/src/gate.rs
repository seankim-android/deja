//! Multi-store, runtime-aware fail-closed release gate evaluator (`DEJA-49`, `DEJA-55`, `DEJA-42`).

use std::collections::HashSet;

use deja_core::{
    GateFlakyFailure, GateGoalResult, GateIdentity, GateOutcome, Goal, WalkIdentity, normalize_goal,
};
use serde::{Deserialize, Serialize};

use crate::consumer_flows::{ConsumerFlows, InspectedConsumerEntry};
use crate::error::StoreError;
use crate::schema::{is_non_empty, is_sha256_hex};
use crate::store::{ClassicFlowCheck, SqliteWalkStore, ValidationRunRecord, validate_runtime};

/// Release gate verdict for a single goal across one or more stores, with shard attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreGateGoalResult {
    pub goal: String,
    pub goal_key: String,
    pub outcome: GateOutcome,
    pub validated_at: Option<String>,
    pub flow_sha256: Option<String>,
    pub closure_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Workspace-relative path of the winning validation run's saved report, if recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
    /// Store labels (`store_id` or workspace-relative DB path) that contributed evidence.
    #[serde(default)]
    pub source_stores: Vec<String>,
}

impl From<StoreGateGoalResult> for GateGoalResult {
    fn from(value: StoreGateGoalResult) -> Self {
        Self {
            goal: value.goal,
            goal_key: value.goal_key,
            outcome: value.outcome,
            validated_at: value.validated_at,
            flow_sha256: value.flow_sha256,
            closure_sha256: value.closure_sha256,
            detail: value.detail,
        }
    }
}

/// Options for multi-store release gate evaluation (`DEJA-49`, `DEJA-55`, `DEJA-42`).
#[derive(Debug, Clone, Default)]
pub struct StoreGateOptions<'a> {
    /// Optional `--runtime` isolation filter (`Some(rt)` filters `runtime_key == rt`;
    /// `None` evaluates across all runtimes fail-closed).
    pub runtime: Option<&'a str>,
    /// Optional required runtime suffix filter (for example Web viewport/mode suffix `"@1280x800"`
    /// or `"@1280x800-headed"`). When `Some(suffix)`, only runs whose recorded `runtime` ends with
    /// `suffix` are eligible — enforced both with and without an explicit `runtime` filter.
    pub runtime_suffix: Option<&'a str>,
    /// External flaky failure markers from run reports (supplementing durable `validation_runs.flaky`).
    pub flaky_failures: &'a [GateFlakyFailure],
    /// Optional consumer manifest for manifest-mode gate checks.
    pub consumer: Option<&'a ConsumerFlows>,
}

/// Returns `true` only when at least one goal was checked and every goal passed.
#[must_use]
pub fn store_gate_passed(results: &[StoreGateGoalResult]) -> bool {
    !results.is_empty() && results.iter().all(|r| r.outcome == GateOutcome::Passed)
}

#[derive(Debug, Clone)]
struct ShardEvidence {
    store_label: String,
    outcome: GateOutcome,
    validated_at: Option<String>,
    flow_sha256: Option<String>,
    closure_sha256: Option<String>,
    detail: Option<String>,
    report_path: Option<String>,
}

fn outcome_severity(outcome: GateOutcome) -> u8 {
    // Fail-closed precedence among evidence-bearing shards:
    // Invalid > FlowChanged > Flaky > Failed > NotRun > Passed
    match outcome {
        GateOutcome::Invalid => 6,
        GateOutcome::FlowChanged => 5,
        GateOutcome::Flaky => 4,
        GateOutcome::Failed => 3,
        GateOutcome::NotRun => 2,
        GateOutcome::Passed => 1,
    }
}

fn validate_gate_identity(identity: &GateIdentity) -> Result<(), StoreError> {
    if !is_non_empty(&identity.app_id)
        || !is_non_empty(&identity.platform)
        || !is_non_empty(&identity.flow_schema_version)
        || !is_sha256_hex(&identity.build_fingerprint)
    {
        return Err(StoreError::InvalidIdentity(
            "Gate identity requires app, platform, flow schema, and SHA-256 build fingerprint"
                .to_string(),
        ));
    }
    if identity
        .adapter
        .as_deref()
        .is_some_and(|a| !is_non_empty(a))
        || identity
            .browser
            .as_deref()
            .is_some_and(|b| !is_non_empty(b))
    {
        return Err(StoreError::InvalidIdentity(
            "Gate adapter and browser must be non-empty when present".to_string(),
        ));
    }
    if identity.platform == "web" && (identity.adapter.is_none() || identity.browser.is_none()) {
        return Err(StoreError::InvalidIdentity(
            "Web gate identity requires explicit adapter and browser names".to_string(),
        ));
    }
    Ok(())
}

/// Evaluate release-gate verdicts across one or more read-only SQLite v5 stores (`DEJA-49`,
/// `DEJA-55`, `DEJA-42`).
///
/// Guarantees:
/// - Strictly read-only: acquires zero writer locks and creates zero files.
/// - Any corrupt, tampered, symlinked, or unreadable store returns `Err(StoreError)` (CLI exit 11).
/// - Deduplicates normalized goals in first-seen order.
/// - Enforces fail-closed merge precedence:
///   `Invalid` (including two `Passed` shards with mismatched `flow_sha256` or `closure_sha256`)
///   > `FlowChanged` > `Flaky` > `Failed` > `NotRun` > `Passed`, recording `source_stores`.
/// - Supports `--runtime` isolation (`Some(rt)` checks only `runtime_key == rt`; `None` checks
///   across all recorded runtimes fail-closed).
pub fn evaluate_gate(
    stores: &[&SqliteWalkStore],
    goals: &[String],
    identity: &GateIdentity,
    options: &StoreGateOptions<'_>,
) -> Result<Vec<StoreGateGoalResult>, StoreError> {
    validate_gate_identity(identity)?;
    let rt_filter = validate_runtime(options.runtime)?;
    let rt_suffix = match options.runtime_suffix {
        None => None,
        Some(sfx) if is_non_empty(sfx) => Some(sfx),
        Some(_) => {
            return Err(StoreError::InvalidIdentity(
                "runtime_suffix must be non-empty when present".to_string(),
            ));
        }
    };

    // Validate every store upfront (read-only: corrupt/tampered/symlinked DBs fail immediately).
    for store in stores {
        store.verify_readonly_connection()?;
    }

    let mut results = Vec::new();
    let mut seen_keys = HashSet::new();

    for raw_goal in goals {
        let Goal { text, key } = normalize_goal(raw_goal)?;
        if !seen_keys.insert(key.clone()) {
            continue;
        }

        let walk_identity = WalkIdentity {
            goal: text.clone(),
            app_id: identity.app_id.clone(),
            platform: identity.platform.clone(),
            flow_schema_version: identity.flow_schema_version.clone(),
            build_fingerprint: identity.build_fingerprint.clone(),
            adapter: identity.adapter.clone(),
            browser: identity.browser.clone(),
        };

        let mut evidence: Vec<ShardEvidence> = Vec::new();

        // If manifest mode is configured, verify disk closure & assertions upfront before checking shards.
        let manifest_closure_filter: Option<(String, String)> =
            if let Some(consumer) = options.consumer {
                // Use first store (or empty check) for manifest resolution; manifest itself is workspace-wide.
                let manifest = consumer.load()?;
                let matched =
                    if identity.flow_schema_version == deja_core::DEFAULT_FLOW_SCHEMA_VERSION {
                        manifest.walks.iter().find(|e| {
                            normalize_goal(&e.goal).ok().map(|g| g.key) == Some(key.clone())
                                && e.app_id == identity.app_id
                                && e.platform == identity.platform
                                && e.adapter.as_str() == identity.adapter.as_deref().unwrap_or("")
                                && e.browser.as_deref() == identity.browser.as_deref()
                        })
                    } else {
                        None
                    };
                match matched {
                    None => {
                        results.push(StoreGateGoalResult {
                            goal: text,
                            goal_key: key,
                            outcome: GateOutcome::NotRun,
                            validated_at: None,
                            flow_sha256: None,
                            closure_sha256: None,
                            detail: Some("no matching entry in walk manifest".to_string()),
                            report_path: None,
                            source_stores: Vec::new(),
                        });
                        continue;
                    }
                    Some(entry) => match consumer.inspect_entry(entry)? {
                        InspectedConsumerEntry::Missing { file } => {
                            results.push(StoreGateGoalResult {
                                goal: text,
                                goal_key: key,
                                outcome: GateOutcome::Invalid,
                                validated_at: None,
                                flow_sha256: None,
                                closure_sha256: None,
                                detail: Some(format!("the flow file is missing: {file}")),
                                report_path: None,
                                source_stores: Vec::new(),
                            });
                            continue;
                        }
                        InspectedConsumerEntry::Ok {
                            closure,
                            main_sha256,
                            assertions_sha256,
                            ..
                        } => {
                            if closure.sha256 != entry.closure_sha256 {
                                results.push(StoreGateGoalResult {
                                    goal: text,
                                    goal_key: key,
                                    outcome: GateOutcome::FlowChanged,
                                    validated_at: None,
                                    flow_sha256: None,
                                    closure_sha256: Some(closure.sha256.clone()),
                                    detail: Some(format!(
                                        "the flow or a file it runs changed (manifest {}, now {})",
                                        entry.closure_sha256, closure.sha256
                                    )),
                                    report_path: None,
                                    source_stores: Vec::new(),
                                });
                                continue;
                            }
                            if assertions_sha256 != entry.assertions_sha256 {
                                results.push(StoreGateGoalResult {
                                    goal: text,
                                    goal_key: key,
                                    outcome: GateOutcome::FlowChanged,
                                    validated_at: None,
                                    flow_sha256: None,
                                    closure_sha256: Some(closure.sha256),
                                    detail: Some(format!(
                                        "the flow's assertions changed (manifest {}, now {})",
                                        entry.assertions_sha256, assertions_sha256
                                    )),
                                    report_path: None,
                                    source_stores: Vec::new(),
                                });
                                continue;
                            }
                            Some((entry.closure_sha256.clone(), main_sha256))
                        }
                    },
                }
            } else {
                None
            };

        for store in stores {
            let store_label = store.store_label();
            // Propagate Err(InvalidWalkMemory) on any tampered/corrupt SQLite row!
            let Some((walk, all_runs)) = store.fetch_logical_walk_and_runs(&walk_identity)? else {
                continue;
            };

            let matching_runs: Vec<ValidationRunRecord> = all_runs
                .into_iter()
                .filter(|run| run.build_fingerprint == identity.build_fingerprint)
                .filter(|run| match rt_filter {
                    Some(rt) => run.runtime.as_deref().unwrap_or("") == rt,
                    None => true,
                })
                .filter(|run| match rt_suffix {
                    Some(suffix) => run.runtime.as_deref().is_some_and(|r| r.ends_with(suffix)),
                    None => true,
                })
                .filter(|run| match &manifest_closure_filter {
                    Some((expected_closure, _)) => {
                        run.closure_sha256.as_deref() == Some(expected_closure.as_str())
                    }
                    None => true,
                })
                .collect();

            for run in matching_runs {
                if !run.passed {
                    let external_flaky = options.flaky_failures.iter().any(|item| {
                        item.goal_key == key
                            && item.build_fingerprint == identity.build_fingerprint
                            && item.at == run.validated_at
                    });
                    let is_flaky = run.flaky || external_flaky;
                    evidence.push(ShardEvidence {
                        store_label: store_label.clone(),
                        outcome: if is_flaky {
                            GateOutcome::Flaky
                        } else {
                            GateOutcome::Failed
                        },
                        validated_at: Some(run.validated_at),
                        flow_sha256: Some(run.flow_sha256),
                        closure_sha256: run.closure_sha256,
                        detail: Some(if is_flaky {
                            "the latest run on this build was flaky".to_string()
                        } else {
                            "the latest validation on this build is a failure".to_string()
                        }),
                        report_path: run.report_path,
                    });
                    continue;
                }

                // Defensive fail-closed: a passing run carrying durable `flaky = 1` never opens
                // the gate, while external `flaky_failures` markers apply only to failed runs.
                if run.flaky {
                    evidence.push(ShardEvidence {
                        store_label: store_label.clone(),
                        outcome: GateOutcome::Flaky,
                        validated_at: Some(run.validated_at),
                        flow_sha256: Some(run.flow_sha256),
                        closure_sha256: run.closure_sha256,
                        detail: Some("the run on this build is marked flaky".to_string()),
                        report_path: run.report_path,
                    });
                    continue;
                }

                if let Some((ref expected_closure, ref main_sha)) = manifest_closure_filter {
                    if &run.flow_sha256 != main_sha {
                        let detail = format!(
                            "recorded run flowSha256 ({}) does not match current manifest main flow ({})",
                            run.flow_sha256, main_sha
                        );
                        evidence.push(ShardEvidence {
                            store_label: store_label.clone(),
                            outcome: GateOutcome::FlowChanged,
                            validated_at: Some(run.validated_at),
                            flow_sha256: Some(run.flow_sha256),
                            closure_sha256: run.closure_sha256,
                            detail: Some(detail),
                            report_path: run.report_path,
                        });
                    } else {
                        evidence.push(ShardEvidence {
                            store_label: store_label.clone(),
                            outcome: GateOutcome::Passed,
                            validated_at: Some(run.validated_at),
                            flow_sha256: Some(main_sha.clone()),
                            closure_sha256: Some(expected_closure.clone()),
                            detail: None,
                            report_path: run.report_path,
                        });
                    }
                } else {
                    match store.verify_classic_stored_flow(&walk, &run)? {
                        ClassicFlowCheck::Ok(_) => {
                            evidence.push(ShardEvidence {
                                store_label: store_label.clone(),
                                outcome: GateOutcome::Passed,
                                validated_at: Some(run.validated_at),
                                flow_sha256: Some(run.flow_sha256),
                                closure_sha256: run.closure_sha256,
                                detail: None,
                                report_path: run.report_path,
                            });
                        }
                        ClassicFlowCheck::Missing => {
                            evidence.push(ShardEvidence {
                                store_label: store_label.clone(),
                                outcome: GateOutcome::Invalid,
                                validated_at: None,
                                flow_sha256: None,
                                closure_sha256: None,
                                detail: Some("the flow file is missing".to_string()),
                                report_path: None,
                            });
                        }
                        ClassicFlowCheck::Legacy => {
                            evidence.push(ShardEvidence {
                                store_label: store_label.clone(),
                                outcome: GateOutcome::NotRun,
                                validated_at: None,
                                flow_sha256: None,
                                closure_sha256: None,
                                detail: Some(
                                    "no passing validation on this build (legacy-closure-unknown)"
                                        .to_string(),
                                ),
                                report_path: None,
                            });
                        }
                        ClassicFlowCheck::Invalid(msg) => {
                            evidence.push(ShardEvidence {
                                store_label: store_label.clone(),
                                outcome: GateOutcome::Invalid,
                                validated_at: None,
                                flow_sha256: None,
                                closure_sha256: None,
                                detail: Some(msg),
                                report_path: None,
                            });
                        }
                        ClassicFlowCheck::FlowChanged {
                            detail,
                            current_closure_sha256,
                            ..
                        } => {
                            evidence.push(ShardEvidence {
                                store_label: store_label.clone(),
                                outcome: GateOutcome::FlowChanged,
                                validated_at: None,
                                flow_sha256: None,
                                closure_sha256: current_closure_sha256,
                                detail: Some(detail),
                                report_path: None,
                            });
                        }
                    }
                }
            }
        }

        results.push(merge_shard_evidence(text, key, evidence));
    }

    Ok(results)
}

fn merge_shard_evidence(
    goal: String,
    goal_key: String,
    evidence: Vec<ShardEvidence>,
) -> StoreGateGoalResult {
    if evidence.is_empty() {
        return StoreGateGoalResult {
            goal,
            goal_key,
            outcome: GateOutcome::NotRun,
            validated_at: None,
            flow_sha256: None,
            closure_sha256: None,
            detail: Some("no passing validation on this build (not-run)".to_string()),
            report_path: None,
            source_stores: Vec::new(),
        };
    }

    let mut source_stores = Vec::new();
    for item in &evidence {
        if !source_stores.contains(&item.store_label) {
            source_stores.push(item.store_label.clone());
        }
    }

    // Cross-shard hash conflict check: two `Passed` shards with mismatched `flow_sha256` or
    // `closure_sha256` MUST fail closed as `Invalid` (highest precedence).
    let passed_shards: Vec<&ShardEvidence> = evidence
        .iter()
        .filter(|e| e.outcome == GateOutcome::Passed)
        .collect();
    if passed_shards.len() >= 2 {
        let first = passed_shards[0];
        if let Some(conflict) = passed_shards.iter().skip(1).find(|other| {
            other.flow_sha256 != first.flow_sha256 || other.closure_sha256 != first.closure_sha256
        }) {
            return StoreGateGoalResult {
                goal,
                goal_key,
                outcome: GateOutcome::Invalid,
                validated_at: None,
                flow_sha256: None,
                closure_sha256: None,
                detail: Some(format!(
                    "conflicting passed flow/closure hashes across stores ({} vs {})",
                    first.store_label, conflict.store_label
                )),
                report_path: None,
                source_stores,
            };
        }
    }

    // Pick the highest-severity fail-closed verdict across all contributing shards.
    let winner = evidence
        .iter()
        .max_by_key(|item| outcome_severity(item.outcome))
        .expect("evidence is non-empty");

    StoreGateGoalResult {
        goal,
        goal_key,
        outcome: winner.outcome,
        validated_at: winner.validated_at.clone(),
        flow_sha256: winner.flow_sha256.clone(),
        closure_sha256: winner.closure_sha256.clone(),
        detail: winner.detail.clone(),
        report_path: winner.report_path.clone(),
        source_stores,
    }
}
