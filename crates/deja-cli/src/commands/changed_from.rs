//! Affected-flow closure selection (`--changed-from <git-ref>`, `DEJA-80`).
//!
//! Resolves repository changes relative to `<git-ref>` (committed, staged, unstaged, and
//! untracked non-ignored files) via asynchronous, cancellable `git` invocations, applies
//! fail-closed full-suite fallback triggers, and selects affected goals by flow closure,
//! `affectedSources` config mappings (goal key, closure glob, or YAML header `tags`), and
//! `--always-include` overrides.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use deja_core::{
    Goal, ManifestWalk, StaleBuildPolicy, StoredWalk, YamlValue, collect_flow_closure,
    normalize_goal, parse_yaml_documents, spelling_key,
};
use deja_store::{ConsumerFlows, SqliteWalkStore, ValidationRunRecord};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::commands::exec_common::GoalTargetFilter;
use crate::commands::manifest::has_unsafe_control_or_bidi_char;
use crate::config::LoadedConfig;
use crate::glob::{CompiledGlob, compile_glob};
use crate::output::{BatchSelectionSummary, SelectionReason, workspace_relative_display};

/// Errors that abort `--changed-from` resolution rather than triggering fail-closed `fallback-all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangedFromError {
    Cancelled,
}

/// Result of `--changed-from` affected-goal selection.
#[derive(Debug, Clone)]
pub struct SelectionOutcome {
    /// Selected goals in deterministic input order paired with their [`SelectionReason`].
    pub selected: Vec<(Goal, SelectionReason)>,
    /// Redactable batch selection metadata for `CliBatchRunReport.selection`.
    pub summary: BatchSelectionSummary,
}

/// Early fail-closed syntactic validation of `--changed-from <GIT_REF>` before invoking `git`.
///
/// Rejects empty/whitespace strings, leading `-` (git option injection such as `--exec=...`),
/// any whitespace, NUL bytes, or unsafe control/bidi/zero-width characters.
pub fn validate_git_ref_syntax(git_ref: &str) -> Result<&str, String> {
    if git_ref.trim().is_empty() {
        return Err("`--changed-from` git ref must not be empty or whitespace".to_string());
    }
    if git_ref.contains('\0') || has_unsafe_control_or_bidi_char(git_ref) {
        return Err(
            "`--changed-from` git ref must not contain NUL, control, line-break, bidi, or zero-width characters"
                .to_string(),
        );
    }
    if git_ref.chars().any(char::is_whitespace) {
        return Err("`--changed-from` git ref must not contain whitespace".to_string());
    }
    if git_ref.starts_with('-') {
        return Err(
            "`--changed-from` git ref must not start with `-` (option injection is forbidden)"
                .to_string(),
        );
    }
    Ok(git_ref)
}

#[derive(Debug)]
enum GitRunError {
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
struct GitChangeSet {
    repo_toplevel: PathBuf,
    changed_ws_files: BTreeSet<String>,
    changed_external_repo_files: BTreeSet<String>,
    changed_repo_files: BTreeSet<String>,
}

impl GitChangeSet {
    fn total_changed_count(&self) -> usize {
        self.changed_ws_files.len() + self.changed_external_repo_files.len()
    }
}

struct CompiledAlwaysInclude {
    normalized_goal_key: Option<String>,
    glob: Option<CompiledGlob>,
}

struct CompiledAffectedSourceRule {
    raw_key: String,
    normalized_goal_key: Option<String>,
    tag_key: String,
    key_glob: Option<CompiledGlob>,
    value_globs: Vec<CompiledGlob>,
}

#[derive(Debug)]
struct ClassicWalkEntry {
    walk: StoredWalk,
    runs: Vec<ValidationRunRecord>,
}

#[derive(Debug)]
struct GoalClosureInfo {
    closure_files: Vec<String>,
    header_tags: Vec<String>,
}

/// Resolve git diff state against `base_ref` and select affected candidate goals.
///
/// Fail-closed invariant: any git/repo/ref error, non-UTF-8 git path, nested gitlink directory,
/// config/manifest/`.maestro/config.{yaml,yml}` edit, or `--fallback-all-on` match runs the full
/// suite with [`SelectionReason::FallbackAll`]. Any individual goal whose flow closure cannot be
/// resolved is fail-closed selected with [`SelectionReason::ChangedClosure`].
#[allow(clippy::too_many_arguments)]
pub async fn resolve_and_select_goals(
    base_ref: &str,
    always_include_raw: &[String],
    fallback_all_on_raw: &[String],
    config: &LoadedConfig,
    consumer: Option<&ConsumerFlows>,
    opened_stores: &[Arc<SqliteWalkStore>],
    build_fingerprint: &str,
    candidate_goals: &[Goal],
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<SelectionOutcome, ChangedFromError> {
    if cancel.is_cancelled() {
        return Err(ChangedFromError::Cancelled);
    }

    let fallback_globs: Vec<CompiledGlob> = fallback_all_on_raw
        .iter()
        .filter_map(|p| compile_glob(p).ok())
        .collect();
    let always_include_rules: Vec<CompiledAlwaysInclude> = always_include_raw
        .iter()
        .map(|raw| CompiledAlwaysInclude {
            normalized_goal_key: normalize_goal(raw).ok().map(|g| g.key),
            glob: compile_glob(raw).ok(),
        })
        .collect();
    let affected_source_rules = compile_affected_source_rules(&config.affected_sources);

    let git_state = collect_git_changes(&config.workspace_root, base_ref, env, cancel).await?;

    let (fallback_all, fallback_reason, changed_files_count, changed_ws_files) = match git_state {
        Ok(changes) => {
            let count = changes.total_changed_count();
            if let Some(reason) = detect_fallback_all_trigger(&changes, config, &fallback_globs) {
                (true, Some(reason), count, changes.changed_ws_files)
            } else {
                (false, None, count, changes.changed_ws_files)
            }
        }
        Err(reason) => (true, Some(reason), 0, BTreeSet::new()),
    };

    if cancel.is_cancelled() {
        return Err(ChangedFromError::Cancelled);
    }

    if fallback_all {
        let selected: Vec<(Goal, SelectionReason)> = candidate_goals
            .iter()
            .cloned()
            .map(|g| (g, SelectionReason::FallbackAll))
            .collect();
        let summary = BatchSelectionSummary {
            base_ref: base_ref.to_string(),
            fallback_all: true,
            fallback_reason,
            changed_files_count,
            selected_count: selected.len(),
            skipped_count: 0,
            skipped_goals: Vec::new(),
        };
        return Ok(SelectionOutcome { selected, summary });
    }

    let selection = select_affected_goals(
        candidate_goals,
        &changed_ws_files,
        config,
        consumer,
        opened_stores,
        build_fingerprint,
        &affected_source_rules,
        &always_include_rules,
    );

    let summary = BatchSelectionSummary {
        base_ref: base_ref.to_string(),
        fallback_all: false,
        fallback_reason: None,
        changed_files_count,
        selected_count: selection.selected.len(),
        skipped_count: selection.skipped_goals.len(),
        skipped_goals: selection.skipped_goals,
    };

    Ok(SelectionOutcome {
        selected: selection.selected,
        summary,
    })
}

struct SelectedAndSkipped {
    selected: Vec<(Goal, SelectionReason)>,
    skipped_goals: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn select_affected_goals(
    candidate_goals: &[Goal],
    changed_ws_files: &BTreeSet<String>,
    config: &LoadedConfig,
    consumer: Option<&ConsumerFlows>,
    opened_stores: &[Arc<SqliteWalkStore>],
    build_fingerprint: &str,
    affected_source_rules: &[CompiledAffectedSourceRule],
    always_include_rules: &[CompiledAlwaysInclude],
) -> SelectedAndSkipped {
    let changed_ws_spelling: HashSet<String> =
        changed_ws_files.iter().map(|p| spelling_key(p)).collect();

    // Preload manifest map once in manifest mode, or stored walks once in classic mode.
    let target_filter = GoalTargetFilter::from_config(config);
    let manifest_by_key: Option<BTreeMap<String, ManifestWalk>> = consumer.and_then(|cf| {
        cf.load().ok().map(|manifest| {
            let mut map = BTreeMap::new();
            for entry in manifest.walks {
                if target_filter.matches_manifest_walk(&entry)
                    && let Ok(norm) = normalize_goal(&entry.goal)
                {
                    map.entry(norm.key).or_insert(entry);
                }
            }
            map
        })
    });

    let mut classic_store_error = false;
    let classic_entries: Vec<ClassicWalkEntry> = if consumer.is_none() {
        let mut all = Vec::new();
        for store in opened_stores {
            match store.list_walks() {
                Ok(walks) => {
                    for walk in walks {
                        if walk.app_id == config.app_id
                            && walk.platform == config.platform
                            && walk.adapter == config.adapter
                            && walk.browser == config.browser
                        {
                            match store.list_validation_runs(&walk.identity()) {
                                Ok(runs) => all.push(ClassicWalkEntry { walk, runs }),
                                Err(_) => {
                                    classic_store_error = true;
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    classic_store_error = true;
                }
            }
        }
        all
    } else {
        Vec::new()
    };

    let mut selected = Vec::new();
    let mut skipped_goals = Vec::new();

    for goal in candidate_goals {
        let closure_info = if classic_store_error {
            None
        } else {
            resolve_goal_closure_info(
                goal,
                config,
                consumer.is_some(),
                manifest_by_key.as_ref(),
                &classic_entries,
                build_fingerprint,
            )
        };

        let Some(info) = closure_info else {
            // Precedence 2: Missing or broken closure -> fail-closed selected.
            selected.push((goal.clone(), SelectionReason::ChangedClosure));
            continue;
        };

        // Precedence 3: Any closure file matches `changed_ws_files` (exact or NFC/case spelling key).
        let closure_changed = info.closure_files.iter().any(|path| {
            changed_ws_files.contains(path) || changed_ws_spelling.contains(&spelling_key(path))
        });
        if closure_changed {
            selected.push((goal.clone(), SelectionReason::ChangedClosure));
            continue;
        }

        // Precedence 4: `affectedSources` rule matching goal key, closure glob, or YAML header tags.
        let matched_affected_source = affected_source_rules.iter().any(|rule| {
            affected_source_rule_matches_goal(rule, goal, &info)
                && rule
                    .value_globs
                    .iter()
                    .any(|vg| changed_ws_files.iter().any(|p| vg.matches(p)))
        });
        if matched_affected_source {
            selected.push((goal.clone(), SelectionReason::ChangedClosure));
            continue;
        }

        // Precedence 5: `--always-include` matching normalized goal key or closure/main flow glob.
        let matched_always_include = always_include_rules.iter().any(|rule| {
            rule.normalized_goal_key.as_deref() == Some(goal.key.as_str())
                || rule
                    .glob
                    .as_ref()
                    .is_some_and(|g| info.closure_files.iter().any(|path| g.matches(path)))
        });
        if matched_always_include {
            selected.push((goal.clone(), SelectionReason::AlwaysInclude));
            continue;
        }

        // Precedence 6: Unselected (skipped cleanly).
        skipped_goals.push(goal.text.clone());
    }

    SelectedAndSkipped {
        selected,
        skipped_goals,
    }
}

fn compile_affected_source_rules(
    map: &BTreeMap<String, Vec<String>>,
) -> Vec<CompiledAffectedSourceRule> {
    let mut rules = Vec::with_capacity(map.len());
    for (raw_key, patterns) in map {
        let normalized_goal_key = normalize_goal(raw_key).ok().map(|g| g.key);
        let tag_key = raw_key
            .strip_prefix("tag:")
            .unwrap_or(raw_key.as_str())
            .trim()
            .to_string();
        let key_glob = if raw_key.starts_with("tag:") {
            None
        } else {
            compile_glob(raw_key).ok()
        };
        let value_globs: Vec<CompiledGlob> = patterns
            .iter()
            .filter_map(|p| compile_glob(p).ok())
            .collect();
        rules.push(CompiledAffectedSourceRule {
            raw_key: raw_key.clone(),
            normalized_goal_key,
            tag_key,
            key_glob,
            value_globs,
        });
    }
    rules
}

fn affected_source_rule_matches_goal(
    rule: &CompiledAffectedSourceRule,
    goal: &Goal,
    info: &GoalClosureInfo,
) -> bool {
    if !rule.raw_key.starts_with("tag:")
        && rule.normalized_goal_key.as_deref() == Some(goal.key.as_str())
    {
        return true;
    }
    if let Some(ref key_glob) = rule.key_glob
        && info.closure_files.iter().any(|p| key_glob.matches(p))
    {
        return true;
    }
    !rule.tag_key.is_empty()
        && info
            .header_tags
            .iter()
            .any(|tag| tag.trim() == rule.tag_key)
}

fn resolve_goal_closure_info(
    goal: &Goal,
    config: &LoadedConfig,
    manifest_mode: bool,
    manifest_by_key: Option<&BTreeMap<String, ManifestWalk>>,
    classic_entries: &[ClassicWalkEntry],
    build_fingerprint: &str,
) -> Option<GoalClosureInfo> {
    if manifest_mode {
        let entry = manifest_by_key?.get(&goal.key)?;
        let flow_abs = config.workspace_root.join(&entry.flow);
        let closure = collect_flow_closure(&config.workspace_root, &flow_abs).ok()?;
        let closure_files: Vec<String> = closure.files.iter().map(|f| f.source.clone()).collect();
        let main_bytes = closure
            .files
            .iter()
            .find(|f| f.path == closure.main)
            .map(|f| f.bytes.as_slice())
            .unwrap_or(&[]);
        let header_tags = extract_flow_header_tags(main_bytes);
        Some(GoalClosureInfo {
            closure_files,
            header_tags,
        })
    } else {
        let matching: Vec<&ClassicWalkEntry> = classic_entries
            .iter()
            .filter(|entry| entry.walk.goal_key == goal.key)
            .collect();
        if matching.is_empty() {
            return None;
        }

        // Collect the union of every replayable validation run's `flow_path` and `subflow_files`
        // across all shard stores and runtimes so editing an earlier runtime's flow closure for a
        // goal recorded across multiple runtimes ALWAYS selects that goal.
        let mut candidate_flow_paths: BTreeSet<String> = BTreeSet::new();
        let mut recorded_subflow_paths: BTreeSet<String> = BTreeSet::new();
        let allow_stale_replay = config.stale_build == Some(StaleBuildPolicy::ReplayLastVerified);

        for entry in &matching {
            if entry.runs.is_empty() {
                if entry.walk.build_fingerprint == build_fingerprint || allow_stale_replay {
                    candidate_flow_paths.insert(entry.walk.flow_path.clone());
                    if let Some(ref subflows) = entry.walk.subflow_files {
                        for sf in subflows {
                            recorded_subflow_paths.insert(sf.path.clone());
                        }
                    }
                }
                continue;
            }

            // 1. Every passing run recorded on the current build fingerprint across ALL runtimes.
            for run in &entry.runs {
                if run.passed && run.build_fingerprint == build_fingerprint {
                    candidate_flow_paths.insert(run.flow_path.clone());
                    if let Some(ref subflows) = run.subflow_files {
                        for sf in subflows {
                            recorded_subflow_paths.insert(sf.path.clone());
                        }
                    }
                }
            }

            // 2. When `staleBuild: "replay-last-verified"` is active, include the latest passing
            //    run per distinct runtime (plus the overall latest passing run) across prior builds.
            if allow_stale_replay {
                let mut latest_by_runtime: BTreeMap<Option<&str>, &ValidationRunRecord> =
                    BTreeMap::new();
                for run in &entry.runs {
                    if !run.passed {
                        continue;
                    }
                    let rt_key = run.runtime.as_deref();
                    latest_by_runtime
                        .entry(rt_key)
                        .and_modify(|existing| {
                            if run.validated_at > existing.validated_at {
                                *existing = run;
                            }
                        })
                        .or_insert(run);
                }
                for run in latest_by_runtime.into_values() {
                    candidate_flow_paths.insert(run.flow_path.clone());
                    if let Some(ref subflows) = run.subflow_files {
                        for sf in subflows {
                            recorded_subflow_paths.insert(sf.path.clone());
                        }
                    }
                }
            }
        }

        if candidate_flow_paths.is_empty() {
            // No replayable passing run exists (e.g., stale build without staleBuild policy or
            // failed validation) -> fail-closed select so `run_batch` surfaces the exact refusal.
            return None;
        }

        let mut closure_files_set = BTreeSet::new();
        let mut header_tags_set = BTreeSet::new();
        for flow_path in &candidate_flow_paths {
            let flow_abs = config.workspace_root.join(flow_path);
            let closure = collect_flow_closure(&config.workspace_root, &flow_abs).ok()?;
            closure_files_set.insert(normalize_rel_posix(flow_path));
            for file in &closure.files {
                closure_files_set.insert(file.source.clone());
            }
            if let Some(main_file) = closure.files.iter().find(|f| f.path == closure.main) {
                for tag in extract_flow_header_tags(&main_file.bytes) {
                    header_tags_set.insert(tag);
                }
            }
        }
        for sf_path in recorded_subflow_paths {
            let norm = normalize_rel_posix(&sf_path);
            if !norm.is_empty() {
                closure_files_set.insert(norm);
            }
        }
        Some(GoalClosureInfo {
            closure_files: closure_files_set.into_iter().collect(),
            header_tags: header_tags_set.into_iter().collect(),
        })
    }
}

/// Extract YAML front-matter `tags` from doc 0 of a `maestro/v1` flow file.
fn extract_flow_header_tags(yaml_bytes: &[u8]) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(yaml_bytes) else {
        return Vec::new();
    };
    let Ok(docs) = parse_yaml_documents(text) else {
        return Vec::new();
    };
    let Some(doc0) = docs.first() else {
        return Vec::new();
    };
    // Only multi-doc flows (or a header mapping in doc 0) carry Maestro front-matter `tags`.
    if docs.len() < 2 && doc0.as_map().is_none() {
        return Vec::new();
    }
    let Some(tags_val) = doc0.get("tags") else {
        return Vec::new();
    };
    match tags_val {
        YamlValue::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }
        }
        YamlValue::Seq(items) => items
            .iter()
            .filter_map(YamlValue::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn matches_path_set(target: &str, exact: &BTreeSet<String>, spelling: &HashSet<String>) -> bool {
    let normalized = normalize_rel_posix(target);
    if normalized.is_empty() {
        return false;
    }
    exact.contains(&normalized) || spelling.contains(&spelling_key(&normalized))
}

fn normalize_rel_posix(raw: &str) -> String {
    let replaced = raw.replace('\\', "/");
    let mut s = replaced.as_str();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.to_string()
}

fn detect_fallback_all_trigger(
    changes: &GitChangeSet,
    config: &LoadedConfig,
    fallback_globs: &[CompiledGlob],
) -> Option<String> {
    let ws_spelling: HashSet<String> = changes
        .changed_ws_files
        .iter()
        .map(|p| spelling_key(p))
        .collect();
    let repo_spelling: HashSet<String> = changes
        .changed_repo_files
        .iter()
        .map(|p| spelling_key(p))
        .collect();

    // 1. Active config file changed (checked workspace-relative, default `deja.config.json`,
    //    AND repo-root-relative via canonical `config_path` vs canonical `show-toplevel`).
    if matches_path_set("deja.config.json", &changes.changed_ws_files, &ws_spelling) {
        return Some("active Déjà configuration file changed".to_string());
    }
    let cfg_ws_rel = workspace_relative_display(&config.workspace_root, &config.config_path);
    if !cfg_ws_rel.starts_with('/')
        && !cfg_ws_rel.split('/').any(|s| s == "..")
        && matches_path_set(&cfg_ws_rel, &changes.changed_ws_files, &ws_spelling)
    {
        return Some("active Déjà configuration file changed".to_string());
    }
    if let (Ok(canon_top), Ok(canon_cfg)) = (
        std::fs::canonicalize(&changes.repo_toplevel),
        std::fs::canonicalize(&config.config_path),
    ) && let Ok(rel) = canon_cfg.strip_prefix(&canon_top)
    {
        let repo_rel = rel.to_string_lossy().replace('\\', "/");
        if matches_path_set(&repo_rel, &changes.changed_repo_files, &repo_spelling) {
            return Some("active Déjà configuration file changed".to_string());
        }
    }

    // 2. Active manifest file changed (`config.manifest`).
    if let Some(ref manifest_rel) = config.manifest {
        if matches_path_set(manifest_rel, &changes.changed_ws_files, &ws_spelling) {
            return Some(format!("active walk manifest `{manifest_rel}` changed"));
        }
        let manifest_abs = config.workspace_root.join(manifest_rel);
        if let (Ok(canon_top), Ok(canon_manifest)) = (
            std::fs::canonicalize(&changes.repo_toplevel),
            std::fs::canonicalize(&manifest_abs),
        ) && let Ok(rel) = canon_manifest.strip_prefix(&canon_top)
        {
            let repo_rel = rel.to_string_lossy().replace('\\', "/");
            if matches_path_set(&repo_rel, &changes.changed_repo_files, &repo_spelling) {
                return Some(format!("active walk manifest `{manifest_rel}` changed"));
            }
        }
    }

    // 3. `.maestro/config.yaml` or `.maestro/config.yml` changed.
    for maestro_cfg in [".maestro/config.yaml", ".maestro/config.yml"] {
        if matches_path_set(maestro_cfg, &changes.changed_ws_files, &ws_spelling) {
            return Some(format!(
                "Maestro workspace configuration `{maestro_cfg}` changed"
            ));
        }
    }

    // 4. Any `--fallback-all-on <glob>` matches ANY path in workspace OR external repo files.
    for glob in fallback_globs {
        let hit = changes.changed_ws_files.iter().any(|p| glob.matches(p))
            || changes
                .changed_external_repo_files
                .iter()
                .any(|p| glob.matches(p))
            || changes.changed_repo_files.iter().any(|p| glob.matches(p));
        if hit {
            return Some(format!(
                "changed file matched `--fallback-all-on` pattern `{}`",
                glob.pattern()
            ));
        }
    }

    None
}

async fn run_git_bytes(
    workspace_root: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<std::process::Output, GitRunError> {
    if cancel.is_cancelled() {
        return Err(GitRunError::Cancelled);
    }

    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(workspace_root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GIT_OPTIONAL_LOCKS", "0");

    if let Some(path) = env.get("PATH") {
        cmd.env("PATH", path);
    }
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
    ] {
        if let Some(val) = env.get(var) {
            cmd.env(var, val);
        } else {
            cmd.env_remove(var);
        }
    }

    let child = cmd
        .spawn()
        .map_err(|err| GitRunError::Failed(format!("failed to invoke git: {err}")))?;

    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(GitRunError::Cancelled),
        res = child.wait_with_output() => match res {
            Ok(out) => Ok(out),
            Err(err) => {
                if cancel.is_cancelled() {
                    Err(GitRunError::Cancelled)
                } else {
                    Err(GitRunError::Failed(format!("git execution failed: {err}")))
                }
            }
        }
    }
}

async fn collect_git_changes(
    workspace_root: &Path,
    base_ref: &str,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<Result<GitChangeSet, String>, ChangedFromError> {
    // 1. Verify git work tree and gather `--show-toplevel` + `--show-prefix`.
    let rev_info = match run_git_bytes(
        workspace_root,
        &[
            "rev-parse",
            "--is-inside-work-tree",
            "--show-toplevel",
            "--show-prefix",
        ],
        env,
        cancel,
    )
    .await
    {
        Ok(out) => out,
        Err(GitRunError::Cancelled) => return Err(ChangedFromError::Cancelled),
        Err(GitRunError::Failed(msg)) => return Ok(Err(msg)),
    };
    if !rev_info.status.success() {
        return Ok(Err("workspace is not inside a git work tree".to_string()));
    }
    let Ok(rev_text) = std::str::from_utf8(&rev_info.stdout) else {
        return Ok(Err("non-UTF-8 output from `git rev-parse`".to_string()));
    };
    let lines: Vec<&str> = rev_text
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    if lines.first().map(|s| s.trim()) != Some("true") {
        return Ok(Err("workspace is not inside a git work tree".to_string()));
    }
    let Some(toplevel_str) = lines.get(1).map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return Ok(Err(
            "`git rev-parse --show-toplevel` returned an empty path".to_string(),
        ));
    };
    let repo_toplevel = PathBuf::from(toplevel_str);
    let raw_prefix = lines.get(2).copied().unwrap_or("");
    let mut repo_prefix = raw_prefix.replace('\\', "/");
    while let Some(stripped) = repo_prefix.strip_prefix('/') {
        repo_prefix = stripped.to_string();
    }
    if !repo_prefix.is_empty() && !repo_prefix.ends_with('/') {
        repo_prefix.push('/');
    }

    // 2. Resolve `<base_ref>^{commit}` strictly to a commit SHA.
    let commit_expr = format!("{base_ref}^{{commit}}");
    let base_out = match run_git_bytes(
        &repo_toplevel,
        &["rev-parse", "--verify", "--quiet", &commit_expr],
        env,
        cancel,
    )
    .await
    {
        Ok(out) => out,
        Err(GitRunError::Cancelled) => return Err(ChangedFromError::Cancelled),
        Err(GitRunError::Failed(msg)) => return Ok(Err(msg)),
    };
    if !base_out.status.success() {
        return Ok(Err(format!(
            "unable to resolve git ref `{base_ref}` to a commit"
        )));
    }
    let Ok(base_sha_raw) = std::str::from_utf8(&base_out.stdout) else {
        return Ok(Err("non-UTF-8 commit SHA from `git rev-parse`".to_string()));
    };
    let base_sha = base_sha_raw.trim();
    if base_sha.is_empty() || base_sha.starts_with('-') {
        return Ok(Err(format!(
            "unable to resolve git ref `{base_ref}` to a valid commit SHA"
        )));
    }

    let mut changed_ws_files = BTreeSet::new();
    let mut changed_external_repo_files = BTreeSet::new();
    let mut changed_repo_files = BTreeSet::new();

    // 3a. Committed changes: run at `repo_toplevel` with `-c diff.relative=false --no-relative`
    //     and `--raw --no-renames -z` so `diff.relative=true` never strips `repo_prefix` and
    //     submodule/gitlink mode `160000` changes always trigger fail-closed `fallback-all`.
    let triple_dot = format!("{base_sha}...HEAD");
    let committed_out = match run_git_bytes(
        &repo_toplevel,
        &[
            "-c",
            "diff.relative=false",
            "-c",
            "diff.renames=false",
            "diff",
            "--raw",
            "--no-renames",
            "--no-relative",
            "-z",
            &triple_dot,
            "--",
        ],
        env,
        cancel,
    )
    .await
    {
        Ok(out) if out.status.success() => out,
        Ok(_) => {
            let double_dot = format!("{base_sha}..HEAD");
            match run_git_bytes(
                &repo_toplevel,
                &[
                    "-c",
                    "diff.relative=false",
                    "-c",
                    "diff.renames=false",
                    "diff",
                    "--raw",
                    "--no-renames",
                    "--no-relative",
                    "-z",
                    &double_dot,
                    "--",
                ],
                env,
                cancel,
            )
            .await
            {
                Ok(out) if out.status.success() => out,
                Ok(_) => {
                    return Ok(Err(format!(
                        "git diff against base ref `{base_ref}` failed"
                    )));
                }
                Err(GitRunError::Cancelled) => return Err(ChangedFromError::Cancelled),
                Err(GitRunError::Failed(msg)) => return Ok(Err(msg)),
            }
        }
        Err(GitRunError::Cancelled) => return Err(ChangedFromError::Cancelled),
        Err(GitRunError::Failed(msg)) => return Ok(Err(msg)),
    };

    if let Err(reason) = ingest_raw_diff_nul_records(
        &committed_out.stdout,
        &repo_prefix,
        &mut changed_ws_files,
        &mut changed_external_repo_files,
        &mut changed_repo_files,
    ) {
        return Ok(Err(reason));
    }

    // 3b. Unstaged working-tree changes (`git -C <toplevel> diff --raw --no-renames --no-relative -z --`).
    let unstaged_out = match run_git_bytes(
        &repo_toplevel,
        &[
            "-c",
            "diff.relative=false",
            "-c",
            "diff.renames=false",
            "diff",
            "--raw",
            "--no-renames",
            "--no-relative",
            "-z",
            "--",
        ],
        env,
        cancel,
    )
    .await
    {
        Ok(out) if out.status.success() => out,
        Ok(_) => {
            return Ok(Err(
                "git diff for unstaged working-tree changes failed".to_string()
            ));
        }
        Err(GitRunError::Cancelled) => return Err(ChangedFromError::Cancelled),
        Err(GitRunError::Failed(msg)) => return Ok(Err(msg)),
    };
    if let Err(reason) = ingest_raw_diff_nul_records(
        &unstaged_out.stdout,
        &repo_prefix,
        &mut changed_ws_files,
        &mut changed_external_repo_files,
        &mut changed_repo_files,
    ) {
        return Ok(Err(reason));
    }

    // 3c. Staged index changes (`git -C <toplevel> diff --cached --raw --no-renames --no-relative -z --`).
    let staged_out = match run_git_bytes(
        &repo_toplevel,
        &[
            "-c",
            "diff.relative=false",
            "-c",
            "diff.renames=false",
            "diff",
            "--cached",
            "--raw",
            "--no-renames",
            "--no-relative",
            "-z",
            "--",
        ],
        env,
        cancel,
    )
    .await
    {
        Ok(out) if out.status.success() => out,
        Ok(_) => {
            return Ok(Err(
                "git diff --cached for staged changes failed".to_string()
            ));
        }
        Err(GitRunError::Cancelled) => return Err(ChangedFromError::Cancelled),
        Err(GitRunError::Failed(msg)) => return Ok(Err(msg)),
    };
    if let Err(reason) = ingest_raw_diff_nul_records(
        &staged_out.stdout,
        &repo_prefix,
        &mut changed_ws_files,
        &mut changed_external_repo_files,
        &mut changed_repo_files,
    ) {
        return Ok(Err(reason));
    }

    // 3d. Untracked non-ignored files across the entire repository (`git -C <toplevel> ls-files
    //     --others --exclude-standard -z`), uniformly partitioned by `repo_prefix` so untracked
    //     repo-root files (e.g. `ci/new.yml` or an external config) are checked against
    //     `--fallback-all-on` and active-config triggers too.
    let untracked_out = match run_git_bytes(
        &repo_toplevel,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        env,
        cancel,
    )
    .await
    {
        Ok(out) if out.status.success() => out,
        Ok(_) => return Ok(Err("git ls-files for untracked files failed".to_string())),
        Err(GitRunError::Cancelled) => return Err(ChangedFromError::Cancelled),
        Err(GitRunError::Failed(msg)) => return Ok(Err(msg)),
    };
    if let Err(reason) = ingest_repo_rel_untracked_nul_paths(
        &untracked_out.stdout,
        &repo_prefix,
        &mut changed_ws_files,
        &mut changed_external_repo_files,
        &mut changed_repo_files,
    ) {
        return Ok(Err(reason));
    }

    Ok(Ok(GitChangeSet {
        repo_toplevel,
        changed_ws_files,
        changed_external_repo_files,
        changed_repo_files,
    }))
}

/// Parse NUL-delimited `git diff --raw --no-renames --no-relative -z` records:
/// `:<old_mode> <new_mode> <old_oid> <new_oid> <status>\0<path>\0`.
///
/// Fails closed (triggering `fallback-all`) on submodule/gitlink mode `160000`, combined/unmerged
/// diffs (`::`), rename/copy entries, malformed records, non-UTF-8 paths, or trailing `/` entries.
fn ingest_raw_diff_nul_records(
    stdout: &[u8],
    repo_prefix: &str,
    changed_ws_files: &mut BTreeSet<String>,
    changed_external_repo_files: &mut BTreeSet<String>,
    changed_repo_files: &mut BTreeSet<String>,
) -> Result<(), String> {
    if stdout.is_empty() {
        return Ok(());
    }
    let mut fields: Vec<&[u8]> = stdout.split(|&b| b == 0).collect();
    if fields.last().is_some_and(|s| s.is_empty()) {
        fields.pop();
    }
    let mut idx = 0usize;
    while idx < fields.len() {
        let header_bytes = fields[idx];
        idx += 1;
        let Ok(header) = std::str::from_utf8(header_bytes) else {
            return Err("non-UTF-8 header in raw git diff output".to_string());
        };
        if header.starts_with("::") {
            return Err("unmerged git conflict / combined diff detected".to_string());
        }
        let Some(rest) = header.strip_prefix(':') else {
            return Err("malformed raw git diff record header".to_string());
        };
        let parts: Vec<&str> = rest.split_ascii_whitespace().collect();
        if parts.len() != 5 {
            return Err("malformed raw git diff metadata fields".to_string());
        }
        let old_mode = parts[0];
        let new_mode = parts[1];
        let status = parts[4];
        if status.starts_with('R') || status.starts_with('C') {
            return Err(format!(
                "unexpected rename/copy status `{status}` in raw git diff"
            ));
        }

        let Some(path_bytes) = fields.get(idx).copied() else {
            return Err("truncated path in raw git diff output".to_string());
        };
        idx += 1;
        if path_bytes.is_empty() {
            return Err("empty path in raw git diff output".to_string());
        }
        let Ok(text) = std::str::from_utf8(path_bytes) else {
            return Err("non-UTF-8 path in git diff output".to_string());
        };
        let path = text.replace('\\', "/");
        if old_mode == "160000" || new_mode == "160000" {
            return Err(format!("submodule/gitlink changed: `{path}`"));
        }
        if path.ends_with('/') {
            return Err(format!(
                "nested git repository or directory entry `{path}` detected in git diff"
            ));
        }
        changed_repo_files.insert(path.clone());
        if repo_prefix.is_empty() {
            changed_ws_files.insert(path);
        } else if let Some(ws_rel) = path.strip_prefix(repo_prefix) {
            if !ws_rel.is_empty() {
                changed_ws_files.insert(ws_rel.to_string());
            }
        } else {
            changed_external_repo_files.insert(path);
        }
    }
    Ok(())
}

/// Skip Déjà's own untracked local runtime bookkeeping files (such as `.deja/memory/walks.sqlite`
/// and `.deja/memory/walks.sqlite.lock` created when shard stores are opened) so repos without
/// `.deja/` in `.gitignore` do not get polluted by ephemeral store/report writes.
/// Tracked diffs and classic stored flows under `.deja/flows/` are NEVER skipped.
fn is_deja_ephemeral_untracked(ws_rel: &str) -> bool {
    ws_rel == ".deja/manifest.lock"
        || ws_rel.starts_with(".deja/memory/")
        || ws_rel.starts_with(".deja/reports/")
        || ws_rel.starts_with(".deja/gate/")
}

fn ingest_repo_rel_untracked_nul_paths(
    stdout: &[u8],
    repo_prefix: &str,
    changed_ws_files: &mut BTreeSet<String>,
    changed_external_repo_files: &mut BTreeSet<String>,
    changed_repo_files: &mut BTreeSet<String>,
) -> Result<(), String> {
    for raw in stdout.split(|&b| b == 0) {
        if raw.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(raw) else {
            return Err("non-UTF-8 path in untracked git files".to_string());
        };
        let path = normalize_rel_posix(text);
        if path.ends_with('/') {
            return Err(format!("untracked nested git repository `{path}` detected"));
        }
        if path.is_empty() {
            continue;
        }
        if repo_prefix.is_empty() {
            if is_deja_ephemeral_untracked(&path) {
                continue;
            }
            changed_repo_files.insert(path.clone());
            changed_ws_files.insert(path);
        } else if let Some(ws_rel) = path.strip_prefix(repo_prefix) {
            if ws_rel.is_empty() || is_deja_ephemeral_untracked(ws_rel) {
                continue;
            }
            changed_repo_files.insert(path.clone());
            changed_ws_files.insert(ws_rel.to_string());
        } else {
            changed_repo_files.insert(path.clone());
            changed_external_repo_files.insert(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_git_ref_syntax_rejects_option_injection_and_unsafe_chars() {
        assert!(validate_git_ref_syntax("origin/main").is_ok());
        assert!(validate_git_ref_syntax("HEAD~1").is_ok());
        assert!(validate_git_ref_syntax("v1.2.3").is_ok());
        assert!(validate_git_ref_syntax("main@{1}").is_ok());

        for bad in [
            "",
            "   ",
            "-n",
            "--exec=evil",
            "--upload-pack=sh",
            "origin main",
            "main\nHEAD",
            "main\0HEAD",
            "main\u{202E}HEAD",
        ] {
            assert!(
                validate_git_ref_syntax(bad).is_err(),
                "expected ref {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn extract_flow_header_tags_reads_seq_and_scalar_forms() {
        let seq_yaml =
            b"appId: com.example.app\ntags:\n  - smoke\n  - payments\n---\n- launchApp\n";
        assert_eq!(
            extract_flow_header_tags(seq_yaml),
            vec!["smoke", "payments"]
        );

        let scalar_yaml = b"appId: com.example.app\ntags: critical\n---\n- launchApp\n";
        assert_eq!(extract_flow_header_tags(scalar_yaml), vec!["critical"]);

        let no_header = b"- launchApp\n- assertVisible: Done\n";
        assert!(extract_flow_header_tags(no_header).is_empty());
    }

    #[test]
    fn ingest_raw_diff_and_untracked_detect_submodules_non_utf8_and_prefix_split() {
        let mut ws = BTreeSet::new();
        let mut ext = BTreeSet::new();
        let mut repo = BTreeSet::new();

        // Standard `--raw -z` records split cleanly across `repo_prefix`.
        let raw_ok = b":100644 100644 aaaaaaa bbbbbbb M\0mobile/flows/a.yaml\0:000000 100644 0000000 ccccccc A\0backend/api.rs\0";
        ingest_raw_diff_nul_records(raw_ok, "mobile/", &mut ws, &mut ext, &mut repo).unwrap();
        assert!(ws.contains("flows/a.yaml"));
        assert!(ext.contains("backend/api.rs"));
        assert_eq!(ws.len(), 1);
        assert_eq!(ext.len(), 1);

        // Submodule / gitlink mode `160000` (added, modified, deleted) fails closed.
        for rec in [
            b":000000 160000 0000000 aaaaaaa A\0submod\0".as_slice(),
            b":160000 160000 aaaaaaa bbbbbbb M\0submod\0".as_slice(),
            b":160000 000000 aaaaaaa 0000000 D\0submod\0".as_slice(),
        ] {
            ws.clear();
            ext.clear();
            repo.clear();
            let err = ingest_raw_diff_nul_records(rec, "", &mut ws, &mut ext, &mut repo)
                .expect_err("gitlink mode 160000 must fail closed");
            assert!(err.contains("submodule/gitlink"), "unexpected error: {err}");
        }

        // Non-UTF-8 path in raw diff fails closed.
        ws.clear();
        ext.clear();
        repo.clear();
        assert!(
            ingest_raw_diff_nul_records(
                b":100644 100644 aaaaaaa bbbbbbb M\0\xff\xfe\0",
                "",
                &mut ws,
                &mut ext,
                &mut repo,
            )
            .is_err()
        );

        // Combined / unmerged diff (`::`) fails closed.
        assert!(
            ingest_raw_diff_nul_records(
                b"::100644 100644 100644 aaa bbb ccc MM\0conflict.yaml\0",
                "",
                &mut ws,
                &mut ext,
                &mut repo,
            )
            .is_err()
        );

        // Untracked repo-root files partition into `ws` vs `ext`, skip `.deja/memory/`,
        // and fail closed on trailing `/` nested git repos.
        ws.clear();
        ext.clear();
        repo.clear();
        ingest_repo_rel_untracked_nul_paths(
            b"mobile/flows/new.yaml\0mobile/.deja/memory/walks.sqlite\0ci/new.yml\0",
            "mobile/",
            &mut ws,
            &mut ext,
            &mut repo,
        )
        .unwrap();
        assert_eq!(ws.into_iter().collect::<Vec<_>>(), vec!["flows/new.yaml"]);
        assert_eq!(ext.into_iter().collect::<Vec<_>>(), vec!["ci/new.yml"]);

        let mut ws2 = BTreeSet::new();
        let mut ext2 = BTreeSet::new();
        let mut repo2 = BTreeSet::new();
        assert!(
            ingest_repo_rel_untracked_nul_paths(
                b"nested-repo/\0",
                "",
                &mut ws2,
                &mut ext2,
                &mut repo2,
            )
            .is_err()
        );
    }
}
