//! `deja manifest check [--untracked] [--json]` — static, read-only, lock-free check that
//! consumer flow files on disk match `deja.walks.json` (`DEJA-43`).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use deja_core::{
    CoreError, ManifestWalk, assertions_sha256, check_manifest_flow_path, collect_flow_closure,
    normalize_goal, parse_walk_manifest,
};
use serde::Serialize;
use serde_json::json;

use crate::CliDeps;
use crate::cli::{ManifestArgs, ManifestCheckArgs, ManifestCommand};
use crate::config::{
    CommandCapability, DEFAULT_CONFIG_PATH, LoadConfigOptions, LoadedConfig, load_config,
};
use crate::exit_codes::{EXIT_MANIFEST_CHECK_FAILED, EXIT_OK, EXIT_USAGE_OR_CONFIG};
use crate::fs_safe::{lexical_normalize, path_inside, read_to_string_nofollow_bounded};
use crate::output::RedactionContext;

const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Returns `true` if `s` contains C0/DEL/C1 controls, bidi controls (including U+061C),
/// Unicode line/paragraph separators, or zero-width formatting characters.
#[must_use]
pub fn has_unsafe_control_or_bidi_char(s: &str) -> bool {
    s.chars().any(is_unsafe_control_or_bidi_char)
}

#[must_use]
pub fn is_unsafe_control_or_bidi_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{0000}'..='\u{001F}'
            | '\u{007F}'..='\u{009F}'
            | '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// Escape unsafe control, bidi, separator, and zero-width characters for safe terminal output.
#[must_use]
pub fn escape_unsafe_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if is_unsafe_control_or_bidi_char(ch) {
            let code = ch as u32;
            if code <= 0xFF {
                out.push_str(&format!("\\x{code:02x}"));
            } else {
                out.push_str(&format!("\\u{code:04x}"));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// POSIX single-quote shell escaping (`sh`, `dash`, `bash`, `zsh`).
#[must_use]
pub fn posix_single_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum EntryStatus {
    Ok,
    FlowChanged,
    AssertionsChanged,
    IdentityMismatch,
    MissingFile,
    ClosureError,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IdentityMismatchDetail {
    field: &'static str,
    manifest: Option<String>,
    config: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestCheckEntry {
    goal: String,
    flow: String,
    status: EntryStatus,
    manifest_closure_sha256: String,
    manifest_assertions_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_closure_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_assertions_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assertions_also_changed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<IdentityMismatchDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "FixField::is_absent")]
    fix: FixField,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix_unsafe: Option<bool>,
}

#[derive(Debug, Clone)]
enum FixField {
    Absent,
    Null,
    Command(String),
}

impl FixField {
    fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl Serialize for FixField {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent | Self::Null => serializer.serialize_none(),
            Self::Command(cmd) => serializer.serialize_str(cmd),
        }
    }
}

pub fn run(
    args: &ManifestArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    match &args.command {
        ManifestCommand::Check(check_args) => {
            run_check(check_args, explicit_config, json_mode, deps)
        }
    }
}

fn run_check(
    args: &ManifestCheckArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    let config = match load_config(&LoadConfigOptions {
        cwd: &deps.cwd,
        explicit_config,
        capability: CommandCapability::Static,
        device_override: None,
        build_override: None,
        env: &deps.env,
    }) {
        Ok(cfg) => cfg,
        Err(err) => {
            let redaction =
                RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);
            emit_manifest_error(
                deps,
                json_mode,
                "CONFIG_ERROR",
                &redaction.redact_str(&err.to_string()),
            );
            return EXIT_USAGE_OR_CONFIG;
        }
    };

    for w in &config.warnings {
        deps.output.writeln_stderr(w);
    }

    let redaction = RedactionContext::new(
        &config.workspace_root,
        deps.home_dir.as_deref(),
        &config.forbidden_values,
        &deps.env,
    );

    let Some(ref manifest_rel) = config.manifest else {
        emit_manifest_error(
            deps,
            json_mode,
            "MANIFEST_NOT_CONFIGURED",
            "`manifest` is not configured in deja.config.json",
        );
        return EXIT_USAGE_OR_CONFIG;
    };

    // Explicitly require the manifest file to exist on disk (absent manifest -> exit 2).
    let manifest_abs = lexical_normalize(&config.workspace_root.join(manifest_rel));
    if !path_inside(&config.workspace_root, &manifest_abs) {
        emit_manifest_error(
            deps,
            json_mode,
            "INVALID_MANIFEST",
            "manifest path escapes workspace",
        );
        return EXIT_USAGE_OR_CONFIG;
    }

    let manifest_raw = match read_to_string_nofollow_bounded(&manifest_abs, MAX_MANIFEST_BYTES) {
        Ok(raw) => raw,
        Err(err) => {
            let msg = if err.kind() == ErrorKind::NotFound {
                format!("manifest file does not exist: {manifest_rel}")
            } else {
                format!("cannot read manifest {manifest_rel}: {err}")
            };
            emit_manifest_error(
                deps,
                json_mode,
                "INVALID_MANIFEST",
                &redaction.redact_str(&msg),
            );
            return EXIT_USAGE_OR_CONFIG;
        }
    };

    if let Ok(canon) = fs::canonicalize(&manifest_abs)
        && !path_inside(&config.workspace_root, &canon)
    {
        emit_manifest_error(
            deps,
            json_mode,
            "INVALID_MANIFEST",
            "manifest path escapes workspace via symlink",
        );
        return EXIT_USAGE_OR_CONFIG;
    }

    let manifest = match parse_walk_manifest(&manifest_raw) {
        Ok(m) => m,
        Err(err) => {
            emit_manifest_error(
                deps,
                json_mode,
                "INVALID_MANIFEST",
                &redaction.redact_str(&err.to_string()),
            );
            return EXIT_USAGE_OR_CONFIG;
        }
    };

    // Determine which goal keys have at least one entry matching this config's identity.
    let mut goals_with_matching_identity: HashSet<String> = HashSet::new();
    for entry in &manifest.walks {
        if let Ok(norm) = normalize_goal(&entry.goal)
            && first_identity_mismatch(entry, &config).is_none()
        {
            goals_with_matching_identity.insert(norm.key);
        }
    }

    let config_flag_arg: Option<&str> = config
        .raw_config_arg
        .as_deref()
        .filter(|arg| *arg != DEFAULT_CONFIG_PATH);

    let mut entries_out = Vec::with_capacity(manifest.walks.len());
    let mut referenced_files: BTreeSet<String> = BTreeSet::new();
    let mut main_flow_dirs: BTreeSet<String> = BTreeSet::new();

    for entry in &manifest.walks {
        if let Some((dir, _)) = entry.flow.rsplit_once('/') {
            main_flow_dirs.insert(dir.to_string());
        } else {
            main_flow_dirs.insert(String::new());
        }

        let goal_key = normalize_goal(&entry.goal)
            .map(|g| g.key)
            .unwrap_or_default();
        let identity_mismatch = if goals_with_matching_identity.contains(&goal_key) {
            None
        } else {
            first_identity_mismatch(entry, &config)
        };

        let checked = check_single_entry(
            entry,
            &deps.cwd,
            &config.workspace_root,
            config_flag_arg,
            identity_mismatch,
            &mut referenced_files,
        );
        entries_out.push(checked);
    }

    let all_ok = entries_out.iter().all(|e| e.status == EntryStatus::Ok);
    let untracked = if args.untracked {
        Some(collect_untracked_flows(
            &config.workspace_root,
            config.flows_dir.as_deref(),
            &main_flow_dirs,
            &referenced_files,
        ))
    } else {
        None
    };

    if json_mode {
        let mut obj = serde_json::Map::new();
        obj.insert("manifest".to_string(), json!(manifest_rel));
        obj.insert("ok".to_string(), json!(all_ok));
        obj.insert(
            "entries".to_string(),
            serde_json::to_value(&entries_out).expect("entries serialize"),
        );
        if let Some(ref list) = untracked {
            obj.insert("untracked".to_string(), json!(list));
        }
        let redacted = redaction.redact_json(&serde_json::Value::Object(obj));
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&redacted).expect("manifest check JSON"));
    } else {
        deps.output.writeln_stdout(&format!(
            "Déjà manifest check ({manifest_rel}): {}",
            if all_ok { "ok" } else { "failed" }
        ));
        for e in &entries_out {
            let safe_goal = escape_unsafe_chars(&redaction.redact_str(&e.goal));
            let safe_flow = escape_unsafe_chars(&redaction.redact_str(&e.flow));
            let status_str = match e.status {
                EntryStatus::Ok => "ok",
                EntryStatus::FlowChanged => "flow-changed",
                EntryStatus::AssertionsChanged => "assertions-changed",
                EntryStatus::IdentityMismatch => "identity-mismatch",
                EntryStatus::MissingFile => "missing-file",
                EntryStatus::ClosureError => "closure-error",
            };
            deps.output
                .writeln_stdout(&format!("  [{status_str}] {safe_goal} ({safe_flow})"));

            if let Some(ref mismatch) = e.identity {
                let safe_manifest = mismatch
                    .manifest
                    .as_deref()
                    .map(|v| escape_unsafe_chars(&redaction.redact_str(v)));
                let safe_config = mismatch
                    .config
                    .as_deref()
                    .map(|v| escape_unsafe_chars(&redaction.redact_str(v)));
                deps.output.writeln_stdout(&format!(
                    "    identity mismatch on {}: manifest={:?}, config={:?}",
                    mismatch.field, safe_manifest, safe_config
                ));
            }
            if let Some(ref missing) = e.file {
                deps.output.writeln_stdout(&format!(
                    "    missing file: {}",
                    escape_unsafe_chars(&redaction.redact_str(missing))
                ));
            }
            if let Some(ref reason) = e.reason {
                deps.output.writeln_stdout(&format!(
                    "    reason: {}",
                    escape_unsafe_chars(&redaction.redact_str(reason))
                ));
            }
            if e.fix_unsafe == Some(true) {
                let manual_cmd = match e.status {
                    EntryStatus::AssertionsChanged => "deja remember --accept-assertion-change",
                    _ => "deja run --verify",
                };
                deps.output.writeln_stdout(&format!(
                    "    fix: entry contains unsafe control/bidi characters; run `{manual_cmd}` for this goal by hand"
                ));
            } else if let FixField::Command(ref cmd) = e.fix {
                deps.output
                    .writeln_stdout(&format!("    fix: {}", redaction.redact_str(cmd)));
                if e.assertions_also_changed == Some(true)
                    && let Some(rem_cmd) = build_remember_fix(
                        &e.goal,
                        &format_flow_path_for_cwd(&deps.cwd, &config.workspace_root, &e.flow),
                        config_flag_arg,
                    )
                {
                    deps.output.writeln_stdout(&format!(
                        "    note: assertions also changed; if --verify refuses with assertions-changed, run: {}",
                        redaction.redact_str(&rem_cmd)
                    ));
                }
            }
        }
        if let Some(ref list) = untracked
            && !list.is_empty()
        {
            deps.output.writeln_stdout("  untracked YAML files:");
            for file in list {
                deps.output.writeln_stdout(&format!(
                    "    - {}",
                    escape_unsafe_chars(&redaction.redact_str(file))
                ));
            }
        }
    }

    if all_ok {
        EXIT_OK
    } else {
        EXIT_MANIFEST_CHECK_FAILED
    }
}

fn first_identity_mismatch(
    entry: &ManifestWalk,
    config: &LoadedConfig,
) -> Option<IdentityMismatchDetail> {
    if entry.app_id != config.app_id {
        return Some(IdentityMismatchDetail {
            field: "appId",
            manifest: Some(entry.app_id.clone()),
            config: Some(config.app_id.clone()),
        });
    }
    if entry.platform != config.platform {
        return Some(IdentityMismatchDetail {
            field: "platform",
            manifest: Some(entry.platform.clone()),
            config: Some(config.platform.clone()),
        });
    }
    if Some(entry.adapter.as_str()) != config.adapter.as_deref() {
        return Some(IdentityMismatchDetail {
            field: "adapter",
            manifest: Some(entry.adapter.clone()),
            config: config.adapter.clone(),
        });
    }
    if entry.browser.as_deref() != config.browser.as_deref() {
        return Some(IdentityMismatchDetail {
            field: "browser",
            manifest: entry.browser.clone(),
            config: config.browser.clone(),
        });
    }
    None
}

fn check_single_entry(
    entry: &ManifestWalk,
    cwd: &Path,
    canonical_workspace: &Path,
    config_flag_arg: Option<&str>,
    identity_mismatch: Option<IdentityMismatchDetail>,
    referenced_files: &mut BTreeSet<String>,
) -> ManifestCheckEntry {
    if let Err(err) = check_manifest_flow_path(&entry.flow, "flow") {
        return ManifestCheckEntry {
            goal: entry.goal.clone(),
            flow: entry.flow.clone(),
            status: EntryStatus::ClosureError,
            manifest_closure_sha256: entry.closure_sha256.clone(),
            manifest_assertions_sha256: entry.assertions_sha256.clone(),
            current_closure_sha256: None,
            current_assertions_sha256: None,
            assertions_also_changed: None,
            identity: None,
            file: None,
            reason: Some(err.to_string()),
            fix: FixField::Absent,
            fix_unsafe: None,
        };
    }

    let abs = lexical_normalize(&canonical_workspace.join(&entry.flow));
    if !path_inside(canonical_workspace, &abs) {
        return ManifestCheckEntry {
            goal: entry.goal.clone(),
            flow: entry.flow.clone(),
            status: EntryStatus::ClosureError,
            manifest_closure_sha256: entry.closure_sha256.clone(),
            manifest_assertions_sha256: entry.assertions_sha256.clone(),
            current_closure_sha256: None,
            current_assertions_sha256: None,
            assertions_also_changed: None,
            identity: None,
            file: None,
            reason: Some(format!("flow escapes workspace: {}", entry.flow)),
            fix: FixField::Absent,
            fix_unsafe: None,
        };
    }

    let canonical_flow = match fs::canonicalize(&abs) {
        Ok(p) => p,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return ManifestCheckEntry {
                goal: entry.goal.clone(),
                flow: entry.flow.clone(),
                status: EntryStatus::MissingFile,
                manifest_closure_sha256: entry.closure_sha256.clone(),
                manifest_assertions_sha256: entry.assertions_sha256.clone(),
                current_closure_sha256: None,
                current_assertions_sha256: None,
                assertions_also_changed: None,
                identity: None,
                file: Some(entry.flow.clone()),
                reason: None,
                fix: FixField::Absent,
                fix_unsafe: None,
            };
        }
        Err(err) => {
            return ManifestCheckEntry {
                goal: entry.goal.clone(),
                flow: entry.flow.clone(),
                status: EntryStatus::ClosureError,
                manifest_closure_sha256: entry.closure_sha256.clone(),
                manifest_assertions_sha256: entry.assertions_sha256.clone(),
                current_closure_sha256: None,
                current_assertions_sha256: None,
                assertions_also_changed: None,
                identity: None,
                file: None,
                reason: Some(err.to_string()),
                fix: FixField::Absent,
                fix_unsafe: None,
            };
        }
    };

    if !path_inside(canonical_workspace, &canonical_flow)
        || !fs::metadata(&canonical_flow)
            .map(|m| m.is_file())
            .unwrap_or(false)
    {
        return ManifestCheckEntry {
            goal: entry.goal.clone(),
            flow: entry.flow.clone(),
            status: EntryStatus::ClosureError,
            manifest_closure_sha256: entry.closure_sha256.clone(),
            manifest_assertions_sha256: entry.assertions_sha256.clone(),
            current_closure_sha256: None,
            current_assertions_sha256: None,
            assertions_also_changed: None,
            identity: None,
            file: None,
            reason: Some(format!(
                "flow escapes workspace or is not a regular file: {}",
                entry.flow
            )),
            fix: FixField::Absent,
            fix_unsafe: None,
        };
    }

    let closure = match collect_flow_closure(canonical_workspace, &canonical_flow) {
        Ok(c) => c,
        Err(CoreError::MissingFlowFile { path }) => {
            return ManifestCheckEntry {
                goal: entry.goal.clone(),
                flow: entry.flow.clone(),
                status: EntryStatus::MissingFile,
                manifest_closure_sha256: entry.closure_sha256.clone(),
                manifest_assertions_sha256: entry.assertions_sha256.clone(),
                current_closure_sha256: None,
                current_assertions_sha256: None,
                assertions_also_changed: None,
                identity: None,
                file: Some(path),
                reason: None,
                fix: FixField::Absent,
                fix_unsafe: None,
            };
        }
        Err(err) => {
            return ManifestCheckEntry {
                goal: entry.goal.clone(),
                flow: entry.flow.clone(),
                status: EntryStatus::ClosureError,
                manifest_closure_sha256: entry.closure_sha256.clone(),
                manifest_assertions_sha256: entry.assertions_sha256.clone(),
                current_closure_sha256: None,
                current_assertions_sha256: None,
                assertions_also_changed: None,
                identity: None,
                file: None,
                reason: Some(err.to_string()),
                fix: FixField::Absent,
                fix_unsafe: None,
            };
        }
    };

    for file in &closure.files {
        referenced_files.insert(file.source.clone());
    }

    let current_assertions = match assertions_sha256(&closure) {
        Ok(hash) => hash,
        Err(err) => {
            return ManifestCheckEntry {
                goal: entry.goal.clone(),
                flow: entry.flow.clone(),
                status: EntryStatus::ClosureError,
                manifest_closure_sha256: entry.closure_sha256.clone(),
                manifest_assertions_sha256: entry.assertions_sha256.clone(),
                current_closure_sha256: Some(closure.sha256),
                current_assertions_sha256: None,
                assertions_also_changed: None,
                identity: None,
                file: None,
                reason: Some(err.to_string()),
                fix: FixField::Absent,
                fix_unsafe: None,
            };
        }
    };

    if closure.sha256 != entry.closure_sha256 {
        let assertions_changed = current_assertions != entry.assertions_sha256;
        let (fix, fix_unsafe) = match build_verify_fix(&entry.goal, config_flag_arg) {
            Some(cmd) => (FixField::Command(cmd), None),
            None => (FixField::Null, Some(true)),
        };
        return ManifestCheckEntry {
            goal: entry.goal.clone(),
            flow: entry.flow.clone(),
            status: EntryStatus::FlowChanged,
            manifest_closure_sha256: entry.closure_sha256.clone(),
            manifest_assertions_sha256: entry.assertions_sha256.clone(),
            current_closure_sha256: Some(closure.sha256),
            current_assertions_sha256: Some(current_assertions),
            assertions_also_changed: if assertions_changed { Some(true) } else { None },
            identity: None,
            file: None,
            reason: None,
            fix,
            fix_unsafe,
        };
    }

    if current_assertions != entry.assertions_sha256 {
        let fix_flow = format_flow_path_for_cwd(cwd, canonical_workspace, &entry.flow);
        let (fix, fix_unsafe) = match build_remember_fix(&entry.goal, &fix_flow, config_flag_arg) {
            Some(cmd) => (FixField::Command(cmd), None),
            None => (FixField::Null, Some(true)),
        };
        return ManifestCheckEntry {
            goal: entry.goal.clone(),
            flow: entry.flow.clone(),
            status: EntryStatus::AssertionsChanged,
            manifest_closure_sha256: entry.closure_sha256.clone(),
            manifest_assertions_sha256: entry.assertions_sha256.clone(),
            current_closure_sha256: Some(closure.sha256),
            current_assertions_sha256: Some(current_assertions),
            assertions_also_changed: None,
            identity: None,
            file: None,
            reason: None,
            fix,
            fix_unsafe,
        };
    }

    if let Some(mismatch) = identity_mismatch {
        return ManifestCheckEntry {
            goal: entry.goal.clone(),
            flow: entry.flow.clone(),
            status: EntryStatus::IdentityMismatch,
            manifest_closure_sha256: entry.closure_sha256.clone(),
            manifest_assertions_sha256: entry.assertions_sha256.clone(),
            current_closure_sha256: Some(closure.sha256),
            current_assertions_sha256: Some(current_assertions),
            assertions_also_changed: None,
            identity: Some(mismatch),
            file: None,
            reason: None,
            fix: FixField::Absent,
            fix_unsafe: None,
        };
    }

    ManifestCheckEntry {
        goal: entry.goal.clone(),
        flow: entry.flow.clone(),
        status: EntryStatus::Ok,
        manifest_closure_sha256: entry.closure_sha256.clone(),
        manifest_assertions_sha256: entry.assertions_sha256.clone(),
        current_closure_sha256: Some(closure.sha256),
        current_assertions_sha256: Some(current_assertions),
        assertions_also_changed: None,
        identity: None,
        file: None,
        reason: None,
        fix: FixField::Absent,
        fix_unsafe: None,
    }
}

fn build_verify_fix(goal: &str, config_flag_arg: Option<&str>) -> Option<String> {
    if has_unsafe_control_or_bidi_char(goal)
        || config_flag_arg.is_some_and(has_unsafe_control_or_bidi_char)
    {
        return None;
    }
    let mut parts = vec!["deja".to_string(), "run".to_string()];
    if let Some(cfg) = config_flag_arg {
        parts.push("--config".to_string());
        parts.push(posix_single_quote(cfg));
    }
    parts.push("--verify".to_string());
    if goal.starts_with('-') {
        parts.push("--".to_string());
    }
    parts.push(posix_single_quote(goal));
    Some(parts.join(" "))
}

/// Compute a path to `workspace_root.join(flow_rel)` relative to `cwd` so runnable
/// `deja remember --flow <path>` fix hints work verbatim when invoked outside `workspace_root`.
#[must_use]
pub fn format_flow_path_for_cwd(cwd: &Path, canonical_workspace: &Path, flow_rel: &str) -> String {
    let abs_flow = lexical_normalize(&canonical_workspace.join(flow_rel));
    let canon_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| lexical_normalize(cwd));
    if let Ok(rel) = abs_flow.strip_prefix(&canon_cwd) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    if let Some(rel) = relative_path_between(&canon_cwd, &abs_flow) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    abs_flow.to_string_lossy().replace('\\', "/")
}

fn relative_path_between(from_dir: &Path, to_path: &Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let from_comps: Vec<Component<'_>> = from_dir.components().collect();
    let to_comps: Vec<Component<'_>> = to_path.components().collect();
    // Require matching roots/prefixes so we never invent a broken cross-drive relative path.
    if from_comps.first() != to_comps.first() {
        return None;
    }
    let common = from_comps
        .iter()
        .zip(to_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return None;
    }
    let mut out = std::path::PathBuf::new();
    for comp in &from_comps[common..] {
        if matches!(comp, Component::Normal(_)) {
            out.push("..");
        }
    }
    for comp in &to_comps[common..] {
        out.push(comp.as_os_str());
    }
    Some(out)
}

fn build_remember_fix(goal: &str, flow: &str, config_flag_arg: Option<&str>) -> Option<String> {
    if has_unsafe_control_or_bidi_char(goal)
        || has_unsafe_control_or_bidi_char(flow)
        || config_flag_arg.is_some_and(has_unsafe_control_or_bidi_char)
    {
        return None;
    }
    let mut parts = vec!["deja".to_string(), "remember".to_string()];
    if let Some(cfg) = config_flag_arg {
        parts.push("--config".to_string());
        parts.push(posix_single_quote(cfg));
    }
    parts.push("--flow".to_string());
    parts.push(posix_single_quote(flow));
    parts.push("--accept-assertion-change".to_string());
    if goal.starts_with('-') {
        parts.push("--".to_string());
    }
    parts.push(posix_single_quote(goal));
    Some(parts.join(" "))
}

fn collect_untracked_flows(
    canonical_workspace: &Path,
    flows_dir: Option<&str>,
    main_flow_dirs: &BTreeSet<String>,
    referenced: &BTreeSet<String>,
) -> Vec<String> {
    let mut untracked = BTreeSet::new();

    if let Some(dir_rel) = flows_dir {
        let dir = lexical_normalize(&canonical_workspace.join(dir_rel));
        scan_yaml_dir(canonical_workspace, &dir, true, referenced, &mut untracked);
    }

    for dir_rel in main_flow_dirs {
        let dir = if dir_rel.is_empty() {
            canonical_workspace.to_path_buf()
        } else {
            lexical_normalize(&canonical_workspace.join(dir_rel))
        };
        scan_yaml_dir(canonical_workspace, &dir, false, referenced, &mut untracked);
    }

    untracked.into_iter().collect()
}

fn scan_yaml_dir(
    canonical_workspace: &Path,
    dir: &Path,
    recursive: bool,
    referenced: &BTreeSet<String>,
    out: &mut BTreeSet<String>,
) {
    let Ok(canon_dir) = fs::canonicalize(dir) else {
        return;
    };
    if canon_dir != canonical_workspace && !path_inside(canonical_workspace, &canon_dir) {
        return;
    }
    let Ok(entries) = fs::read_dir(&canon_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(lmeta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if lmeta.file_type().is_symlink() {
            continue;
        }
        if lmeta.file_type().is_dir() {
            if recursive {
                // Skip hidden `.deja` directory even if scanned.
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(".deja"))
                {
                    continue;
                }
                scan_yaml_dir(canonical_workspace, &path, true, referenced, out);
            }
        } else if lmeta.file_type().is_file() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let lower = name_str.to_ascii_lowercase();
            if !(lower.ends_with(".yaml") || lower.ends_with(".yml")) {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(canonical_workspace) {
                let rel_posix = rel.to_string_lossy().replace('\\', "/");
                if !referenced.contains(&rel_posix) {
                    out.insert(rel_posix);
                }
            }
        }
    }
}

fn emit_manifest_error(deps: &CliDeps, json_mode: bool, code: &str, message: &str) {
    if json_mode {
        let payload = json!({
            "error": { "code": code, "message": message }
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("error JSON"));
    } else {
        deps.output
            .writeln_stderr(&format!("deja manifest check error: {message}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidi_and_control_chars_flag_unsafe_and_escape_cleanly() {
        assert!(has_unsafe_control_or_bidi_char("safe\u{202e}evil"));
        assert!(has_unsafe_control_or_bidi_char("safe\u{061c}rtl"));
        assert!(has_unsafe_control_or_bidi_char("safe\u{200b}zwsp"));
        assert!(has_unsafe_control_or_bidi_char("safe\x1b[31m"));
        assert!(!has_unsafe_control_or_bidi_char("Complete checkout (iOS)"));
        assert_eq!(escape_unsafe_chars("a\x1bb\u{202e}c"), "a\\x1bb\\u202ec");
        assert!(build_verify_fix("bad\u{202e}goal", None).is_none());
        assert_eq!(
            build_verify_fix("-dash-goal", None).as_deref(),
            Some("deja run --verify -- -dash-goal")
        );
        assert_eq!(
            build_verify_fix("it's ok", Some("my config.json")).as_deref(),
            Some("deja run --config 'my config.json' --verify 'it'\\''s ok'")
        );
    }
}
