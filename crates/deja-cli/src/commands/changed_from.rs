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
    Goal, ManifestWalk, StoredWalk, YamlValue, collect_flow_closure, normalize_goal,
    parse_yaml_documents, spelling_key,
};
use deja_store::{ConsumerFlows, SqliteWalkStore};
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
    glob: CompiledGlob,
}

struct CompiledAffectedSourceRule {
    raw_key: String,
    normalized_goal_key: Option<String>,
    tag_key: String,
    key_glob: Option<CompiledGlob>,
    value_globs: Vec<CompiledGlob>,
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
        .filter_map(|raw| {
            let glob = compile_glob(raw).ok()?;
            let normalized_goal_key = normalize_goal(raw).ok().map(|g| g.key);
            Some(CompiledAlwaysInclude {
                normalized_goal_key,
                glob,
            })
        })
        .collect();
    let affected_source_rules = compile_affected_source_rules(&config.affected_sources);

    let git_state = collect_git_changes(
        &config.workspace_root,
        &config.config_path,
        base_ref,
        env,
        cancel,
    )
    .await?;

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

    let classic_walks: Vec<StoredWalk> = if consumer.is_none() {
        let mut all = Vec::new();
        for store in opened_stores {
            if let Ok(walks) = store.list_walks() {
                for w in walks {
                    if w.app_id == config.app_id
                        && w.platform == config.platform
                        && w.adapter == config.adapter
                        && w.browser == config.browser
                    {
                        all.push(w);
                    }
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
        let closure_info = resolve_goal_closure_info(
            goal,
            config,
            consumer.is_some(),
            manifest_by_key.as_ref(),
            &classic_walks,
            build_fingerprint,
        );

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
                || info
                    .closure_files
                    .iter()
                    .any(|path| rule.glob.matches(path))
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
    classic_walks: &[StoredWalk],
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
        let matching: Vec<&StoredWalk> = classic_walks
            .iter()
            .filter(|w| w.goal_key == goal.key)
            .collect();
        if matching.is_empty() {
            return None;
        }
        // Prefer walks for the active build fingerprint; otherwise fall back to the most recently
        // validated walk(s) (supporting `staleBuild: "replay-last-verified"`).
        let exact_build: Vec<&StoredWalk> = matching
            .iter()
            .copied()
            .filter(|w| w.build_fingerprint == build_fingerprint)
            .collect();
        let selected_walks: Vec<&StoredWalk> = if !exact_build.is_empty() {
            exact_build
        } else {
            let mut sorted = matching;
            sorted.sort_by(|a, b| b.last_validation.at.cmp(&a.last_validation.at));
            vec![sorted[0]]
        };

        let mut closure_files_set = BTreeSet::new();
        let mut header_tags_set = BTreeSet::new();
        for walk in selected_walks {
            let flow_abs = config.workspace_root.join(&walk.flow_path);
            let closure = collect_flow_closure(&config.workspace_root, &flow_abs).ok()?;
            for file in &closure.files {
                closure_files_set.insert(file.source.clone());
            }
            if let Some(main_file) = closure.files.iter().find(|f| f.path == closure.main) {
                for tag in extract_flow_header_tags(&main_file.bytes) {
                    header_tags_set.insert(tag);
                }
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
    config_path: &Path,
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
        workspace_root,
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

    // 3a. Committed changes: try `<base_sha>...HEAD --`, fallback `<base_sha>..HEAD --`.
    let triple_dot = format!("{base_sha}...HEAD");
    let committed_out = match run_git_bytes(
        workspace_root,
        &[
            "diff",
            "--name-only",
            "--no-renames",
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
                workspace_root,
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
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

    if let Err(reason) = ingest_repo_rel_nul_paths(
        &committed_out.stdout,
        &repo_prefix,
        &mut changed_ws_files,
        &mut changed_external_repo_files,
        &mut changed_repo_files,
    ) {
        return Ok(Err(reason));
    }

    // 3b. Unstaged working-tree changes (`git diff --name-only --no-renames -z --`).
    let unstaged_out = match run_git_bytes(
        workspace_root,
        &["diff", "--name-only", "--no-renames", "-z", "--"],
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
    if let Err(reason) = ingest_repo_rel_nul_paths(
        &unstaged_out.stdout,
        &repo_prefix,
        &mut changed_ws_files,
        &mut changed_external_repo_files,
        &mut changed_repo_files,
    ) {
        return Ok(Err(reason));
    }

    // 3c. Staged index changes (`git diff --cached --name-only --no-renames -z --`).
    let staged_out = match run_git_bytes(
        workspace_root,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--no-renames",
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
    if let Err(reason) = ingest_repo_rel_nul_paths(
        &staged_out.stdout,
        &repo_prefix,
        &mut changed_ws_files,
        &mut changed_external_repo_files,
        &mut changed_repo_files,
    ) {
        return Ok(Err(reason));
    }

    // 3d. Untracked non-ignored files (`git ls-files --others --exclude-standard -z`,
    //     already relative to `workspace_root`).
    let untracked_out = match run_git_bytes(
        workspace_root,
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
    if let Err(reason) = ingest_ws_rel_untracked_nul_paths(
        &untracked_out.stdout,
        &repo_prefix,
        &mut changed_ws_files,
        &mut changed_repo_files,
    ) {
        return Ok(Err(reason));
    }

    // 3e. If the active config file lives inside the git repo but outside a subdirectory workspace,
    //     check O(1) whether that specific config file is untracked (non-ignored) at repo root.
    if !repo_prefix.is_empty()
        && let (Ok(canon_top), Ok(canon_cfg)) = (
            std::fs::canonicalize(&repo_toplevel),
            std::fs::canonicalize(config_path),
        )
        && let Ok(rel) = canon_cfg.strip_prefix(&canon_top)
    {
        let cfg_repo_rel = rel.to_string_lossy().replace('\\', "/");
        if !cfg_repo_rel.is_empty() && !cfg_repo_rel.starts_with(&repo_prefix) {
            let ext_cfg_out = match run_git_bytes(
                &canon_top,
                &[
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    &cfg_repo_rel,
                ],
                env,
                cancel,
            )
            .await
            {
                Ok(out) if out.status.success() => out,
                Ok(_) => {
                    return Ok(Err(
                        "git ls-files check for external configuration file failed".to_string(),
                    ));
                }
                Err(GitRunError::Cancelled) => return Err(ChangedFromError::Cancelled),
                Err(GitRunError::Failed(msg)) => return Ok(Err(msg)),
            };
            if let Err(reason) = ingest_repo_rel_nul_paths(
                &ext_cfg_out.stdout,
                &repo_prefix,
                &mut changed_ws_files,
                &mut changed_external_repo_files,
                &mut changed_repo_files,
            ) {
                return Ok(Err(reason));
            }
        }
    }

    Ok(Ok(GitChangeSet {
        repo_toplevel,
        changed_ws_files,
        changed_external_repo_files,
        changed_repo_files,
    }))
}

fn ingest_repo_rel_nul_paths(
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
            return Err("non-UTF-8 path in git diff output".to_string());
        };
        let path = text.replace('\\', "/");
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

fn ingest_ws_rel_untracked_nul_paths(
    stdout: &[u8],
    repo_prefix: &str,
    changed_ws_files: &mut BTreeSet<String>,
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
        if path.is_empty() || is_deja_ephemeral_untracked(&path) {
            continue;
        }
        let repo_rel = if repo_prefix.is_empty() {
            path.clone()
        } else {
            format!("{repo_prefix}{path}")
        };
        changed_repo_files.insert(repo_rel);
        changed_ws_files.insert(path);
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
    fn ingest_nul_paths_rejects_non_utf8_and_nested_gitlinks() {
        let mut ws = BTreeSet::new();
        let mut ext = BTreeSet::new();
        let mut repo = BTreeSet::new();

        // Non-UTF-8 bytes fail closed.
        assert!(
            ingest_repo_rel_nul_paths(b"good.yaml\0\xff\xfe\0", "", &mut ws, &mut ext, &mut repo)
                .is_err()
        );

        // Trailing `/` nested gitlink in untracked files fails closed.
        ws.clear();
        repo.clear();
        assert!(
            ingest_ws_rel_untracked_nul_paths(b"nested-repo/\0", "", &mut ws, &mut repo).is_err()
        );

        // Subdirectory prefix splits workspace vs external paths cleanly.
        ws.clear();
        ext.clear();
        repo.clear();
        ingest_repo_rel_nul_paths(
            b"mobile/flows/a.yaml\0backend/api.rs\0",
            "mobile/",
            &mut ws,
            &mut ext,
            &mut repo,
        )
        .unwrap();
        assert!(ws.contains("flows/a.yaml"));
        assert!(ext.contains("backend/api.rs"));
        assert_eq!(ws.len(), 1);
        assert_eq!(ext.len(), 1);
    }
}
