//! Committed walk manifest (`deja.walks.json`) parsing, deterministic serialization,
//! duplicate-key rejection, `DEJA-38` N4 path spelling checks, and `DEJA-57` owner rules.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::canonical_json::{cmp_utf16, js_array_index, js_json_number_to_string};
use crate::contracts::{normalize_goal, trim_es_whitespace};
use crate::error::CoreError;
use crate::flow_assertions::ClosureAssertion;
use crate::flow_closure::{FlowClosure, spelling_key};
use crate::flow_references::FlowReferenceKind;
use crate::yaml::{YamlValue, parse_yaml_documents};

/// Supported `deja.walks.json` manifest format version.
pub const WALK_MANIFEST_VERSION: u32 = 1;

/// Supported closure and assertion hash version recorded in the manifest.
pub const MANIFEST_HASH_VERSION: u32 = 1;

/// Owner-configured guardrails on a manifest walk (`minAssertions`, `requiredAssertionIds`,
/// and `DEJA-57` `disallowTextSelectors` / `allowedTextSelectors`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestOwnerRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_assertions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_assertion_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallow_text_selectors: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_text_selectors: Option<Vec<String>>,
}

/// Scalar launch argument value permitted inside [`ManifestProfile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LaunchArgumentValue {
    String(String),
    Number(Number),
    Bool(bool),
}

/// Launch settings recorded with a manifest walk (never secrets or home paths).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_arguments: Option<BTreeMap<String, LaunchArgumentValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_state: Option<bool>,
}

/// One walk entry in `deja.walks.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestWalk {
    pub goal: String,
    pub app_id: String,
    pub platform: String,
    pub adapter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    pub flow: String,
    pub closure_sha256: String,
    pub assertions_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_rules: Option<ManifestOwnerRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ManifestProfile>,
}

/// Top-level committed walk manifest (`deja.walks.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkManifest {
    pub manifest_version: u32,
    pub hash_version: u32,
    pub walks: Vec<ManifestWalk>,
}

/// A disallowed visible-text-only selector found under `ownerRules.disallowTextSelectors: true` (`DEJA-57`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSelectorViolation {
    pub file: String,
    pub command: String,
    pub selector: String,
}

/// Return an empty version-1 walk manifest.
#[must_use]
pub fn empty_walk_manifest() -> WalkManifest {
    WalkManifest {
        manifest_version: WALK_MANIFEST_VERSION,
        hash_version: MANIFEST_HASH_VERSION,
        walks: Vec::new(),
    }
}

fn sha_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-f0-9]{64}$").expect("valid sha regex"))
}

fn home_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(^|[\s"'=:])(/Users/|/home/|/var/|/private/|~/|[A-Za-z]:[\\/])"#)
            .expect("valid home path regex")
    })
}

fn secret_name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)pass(word|wd)?|secret|token|api[-_]?key|credential|private[-_]?key")
            .expect("valid secret name regex")
    })
}

fn windows_drive_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z]:").expect("valid drive prefix regex"))
}

fn yaml_ext_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\.ya?ml$").expect("valid yaml ext regex"))
}

fn has_control_char(value: &str) -> bool {
    value
        .chars()
        .any(|c| matches!(c, '\u{0000}'..='\u{001f}' | '\u{007f}'))
}

fn only_keys(
    obj: &Map<String, Value>,
    allowed: &[&str],
    where_label: &str,
) -> Result<(), CoreError> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(CoreError::InvalidManifest(format!(
                "{where_label} has unknown field: {key}"
            )));
        }
    }
    Ok(())
}

fn read_text(value: Option<&Value>, where_label: &str) -> Result<String, CoreError> {
    let Some(Value::String(s)) = value else {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must be a non-empty string"
        )));
    };
    if trim_es_whitespace(s).is_empty() || s.contains('\0') {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must be a non-empty string"
        )));
    }
    if has_control_char(s) {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must not contain control characters"
        )));
    }
    if s.starts_with('/') || home_path_regex().is_match(s) {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} looks like an absolute or home path"
        )));
    }
    Ok(s.clone())
}

/// Validate a workspace-relative `/`-separated YAML flow path outside `.deja/`.
pub fn check_manifest_flow_path(value: &str, where_label: &str) -> Result<String, CoreError> {
    if value.is_empty()
        || value.contains('\0')
        || has_control_char(value)
        || value.contains('\\')
        || value.starts_with('/')
        || windows_drive_prefix_regex().is_match(value)
    {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must be a relative path with / separators"
        )));
    }
    let segments: Vec<&str> = value.split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must not hold empty, . or .. segments: {value}"
        )));
    }
    if segments[0].eq_ignore_ascii_case(".deja") {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must be the consumer's own file, not under .deja/: {value}"
        )));
    }
    if !yaml_ext_regex().is_match(value) {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must be a YAML file: {value}"
        )));
    }
    Ok(value.to_string())
}

/// Identity key for a manifest walk: normalized goal + app + platform + adapter + browser.
pub fn manifest_walk_key(walk: &ManifestWalk) -> Result<String, CoreError> {
    let goal_key = normalize_goal(&walk.goal)?.key;
    let tuple = Value::Array(vec![
        Value::String(goal_key),
        Value::String(walk.app_id.clone()),
        Value::String(walk.platform.clone()),
        Value::String(walk.adapter.clone()),
        match &walk.browser {
            Some(browser) => Value::String(browser.clone()),
            None => Value::Null,
        },
    ]);
    Ok(serde_json::to_string(&tuple).expect("manifest walk key tuple serializes"))
}

/// Extract a non-negative integral JSON number by mathematical value (`1`, `1.0`, `1e0`),
/// matching JavaScript `Number.isInteger(n) && n >= 0`.
fn json_non_negative_integer(value: &Value) -> Option<u64> {
    let num = value.as_number()?;
    if let Some(u) = num.as_u64() {
        return Some(u);
    }
    if let Some(i) = num.as_i64() {
        return u64::try_from(i).ok();
    }
    let f = num.as_f64()?;
    if f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f <= 9_007_199_254_740_991.0 {
        Some(f as u64)
    } else {
        None
    }
}

fn read_owner_rules(value: &Value, where_label: &str) -> Result<ManifestOwnerRules, CoreError> {
    let Some(obj) = value.as_object() else {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must be an object"
        )));
    };
    only_keys(
        obj,
        &[
            "minAssertions",
            "requiredAssertionIds",
            "disallowTextSelectors",
            "allowedTextSelectors",
        ],
        where_label,
    )?;

    let mut rules = ManifestOwnerRules::default();

    if let Some(min_val) = obj.get("minAssertions") {
        let Some(min_int) = json_non_negative_integer(min_val) else {
            return Err(CoreError::InvalidManifest(format!(
                "{where_label}.minAssertions must be a non-negative integer"
            )));
        };
        rules.min_assertions = Some(min_int);
    }

    if let Some(req_val) = obj.get("requiredAssertionIds") {
        let Some(items) = req_val.as_array() else {
            return Err(CoreError::InvalidManifest(format!(
                "{where_label}.requiredAssertionIds must be an array"
            )));
        };
        let mut ids = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            ids.push(read_text(
                Some(item),
                &format!("{where_label}.requiredAssertionIds[{index}]"),
            )?);
        }
        rules.required_assertion_ids = Some(ids);
    }

    if let Some(disallow_val) = obj.get("disallowTextSelectors") {
        let Some(flag) = disallow_val.as_bool() else {
            return Err(CoreError::InvalidManifest(format!(
                "{where_label}.disallowTextSelectors must be a boolean"
            )));
        };
        rules.disallow_text_selectors = Some(flag);
    }

    if let Some(allowed_val) = obj.get("allowedTextSelectors") {
        let Some(items) = allowed_val.as_array() else {
            return Err(CoreError::InvalidManifest(format!(
                "{where_label}.allowedTextSelectors must be an array"
            )));
        };
        let mut allowed = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            allowed.push(read_text(
                Some(item),
                &format!("{where_label}.allowedTextSelectors[{index}]"),
            )?);
        }
        rules.allowed_text_selectors = Some(allowed);
    }

    Ok(rules)
}

fn read_profile(value: &Value, where_label: &str) -> Result<ManifestProfile, CoreError> {
    let Some(obj) = value.as_object() else {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must be an object"
        )));
    };
    only_keys(obj, &["name", "launchArguments", "clearState"], where_label)?;

    let mut profile = ManifestProfile::default();
    if let Some(name_val) = obj.get("name") {
        profile.name = Some(read_text(Some(name_val), &format!("{where_label}.name"))?);
    }
    if let Some(clear_val) = obj.get("clearState") {
        let Some(flag) = clear_val.as_bool() else {
            return Err(CoreError::InvalidManifest(format!(
                "{where_label}.clearState must be a boolean"
            )));
        };
        profile.clear_state = Some(flag);
    }
    if let Some(args_val) = obj.get("launchArguments") {
        let Some(args_obj) = args_val.as_object() else {
            return Err(CoreError::InvalidManifest(format!(
                "{where_label}.launchArguments must be an object"
            )));
        };
        let mut launch_arguments = BTreeMap::new();
        for (key, arg) in args_obj {
            if secret_name_regex().is_match(key) {
                return Err(CoreError::InvalidManifest(format!(
                    "{where_label}.launchArguments.{key} looks like a secret"
                )));
            }
            let arg_where = format!("{where_label}.launchArguments.{key}");
            let converted = match arg {
                Value::String(_) => LaunchArgumentValue::String(read_text(Some(arg), &arg_where)?),
                Value::Bool(b) => LaunchArgumentValue::Bool(*b),
                Value::Number(n) if n.as_f64().is_some_and(f64::is_finite) => {
                    LaunchArgumentValue::Number(n.clone())
                }
                _ => {
                    return Err(CoreError::InvalidManifest(format!(
                        "{arg_where} must be a string, number or boolean"
                    )));
                }
            };
            launch_arguments.insert(key.clone(), converted);
        }
        profile.launch_arguments = Some(launch_arguments);
    }

    Ok(profile)
}

fn read_walk(value: &Value, index: usize) -> Result<ManifestWalk, CoreError> {
    let where_label = format!("walks[{index}]");
    let Some(obj) = value.as_object() else {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} must be an object"
        )));
    };
    only_keys(
        obj,
        &[
            "goal",
            "appId",
            "platform",
            "adapter",
            "browser",
            "flow",
            "closureSha256",
            "assertionsSha256",
            "ownerRules",
            "profile",
        ],
        &where_label,
    )?;

    let goal = read_text(obj.get("goal"), &format!("{where_label}.goal"))?;
    if normalize_goal(&goal).is_err() {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label}.goal must not be empty"
        )));
    }

    let platform = read_text(obj.get("platform"), &format!("{where_label}.platform"))?;
    if !matches!(platform.as_str(), "ios" | "android" | "web") {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label}.platform must be ios, android or web"
        )));
    }

    let closure_sha = obj
        .get("closureSha256")
        .and_then(Value::as_str)
        .unwrap_or("");
    let assertions_sha = obj
        .get("assertionsSha256")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !sha_regex().is_match(closure_sha) || !sha_regex().is_match(assertions_sha) {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label} needs lowercase hex SHA-256 closureSha256 and assertionsSha256"
        )));
    }

    let has_browser = obj.contains_key("browser");
    if (platform == "web") != has_browser {
        return Err(CoreError::InvalidManifest(format!(
            "{where_label}.browser is required for web and not allowed otherwise"
        )));
    }

    let browser = if has_browser {
        Some(read_text(
            obj.get("browser"),
            &format!("{where_label}.browser"),
        )?)
    } else {
        None
    };

    let flow_raw = match obj.get("flow") {
        Some(Value::String(s)) => s.as_str(),
        _ => {
            return Err(CoreError::InvalidManifest(format!(
                "{where_label}.flow must be a relative path with / separators"
            )));
        }
    };
    let flow = check_manifest_flow_path(flow_raw, &format!("{where_label}.flow"))?;

    let owner_rules = match obj.get("ownerRules") {
        Some(val) => Some(read_owner_rules(val, &format!("{where_label}.ownerRules"))?),
        None => None,
    };

    let profile = match obj.get("profile") {
        Some(val) => Some(read_profile(val, &format!("{where_label}.profile"))?),
        None => None,
    };

    Ok(ManifestWalk {
        goal,
        app_id: read_text(obj.get("appId"), &format!("{where_label}.appId"))?,
        platform,
        adapter: read_text(obj.get("adapter"), &format!("{where_label}.adapter"))?,
        browser,
        flow,
        closure_sha256: closure_sha.to_string(),
        assertions_sha256: assertions_sha.to_string(),
        owner_rules,
        profile,
    })
}

/// Find the first duplicate key inside any JSON object at any nesting depth.
///
/// Assumes `text` has already been validated as syntactically valid JSON.
fn find_duplicate_json_key(text: &str) -> Option<String> {
    struct Scanner<'a> {
        src: &'a str,
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> Scanner<'a> {
        fn skip_ws(&mut self) {
            while self.pos < self.bytes.len()
                && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n')
            {
                self.pos += 1;
            }
        }

        fn scan_string(&mut self) -> Option<String> {
            let start = self.pos;
            if self.pos >= self.bytes.len() || self.bytes[self.pos] != b'"' {
                return None;
            }
            self.pos += 1;
            while self.pos < self.bytes.len() && self.bytes[self.pos] != b'"' {
                if self.bytes[self.pos] == b'\\' {
                    self.pos += 2;
                } else {
                    self.pos += 1;
                }
            }
            if self.pos < self.bytes.len() {
                self.pos += 1;
            }
            serde_json::from_str::<String>(&self.src[start..self.pos]).ok()
        }

        fn scan_value(&mut self) -> Option<String> {
            self.skip_ws();
            let ch = *self.bytes.get(self.pos)?;
            match ch {
                b'{' => {
                    self.pos += 1;
                    let mut seen = HashSet::new();
                    self.skip_ws();
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'}' {
                        self.skip_ws();
                        let key = self.scan_string()?;
                        if !seen.insert(key.clone()) {
                            return Some(key);
                        }
                        self.skip_ws();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b':' {
                            self.pos += 1;
                        }
                        if let Some(dup) = self.scan_value() {
                            return Some(dup);
                        }
                        self.skip_ws();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b',' {
                            self.pos += 1;
                        }
                        self.skip_ws();
                    }
                    if self.pos < self.bytes.len() {
                        self.pos += 1;
                    }
                    None
                }
                b'[' => {
                    self.pos += 1;
                    self.skip_ws();
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b']' {
                        if let Some(dup) = self.scan_value() {
                            return Some(dup);
                        }
                        self.skip_ws();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b',' {
                            self.pos += 1;
                        }
                        self.skip_ws();
                    }
                    if self.pos < self.bytes.len() {
                        self.pos += 1;
                    }
                    None
                }
                b'"' => {
                    let _ = self.scan_string();
                    None
                }
                _ => {
                    while self.pos < self.bytes.len()
                        && !matches!(
                            self.bytes[self.pos],
                            b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'
                        )
                    {
                        self.pos += 1;
                    }
                    None
                }
            }
        }
    }

    let mut scanner = Scanner {
        src: text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    scanner.scan_value()
}

/// Parse and strictly validate a `deja.walks.json` manifest.
///
/// Fails closed on invalid JSON, duplicate JSON keys at any nesting depth, unknown versions or
/// fields, unsafe paths, home paths, control characters, secret-like launch argument names,
/// duplicate walk identities, and `DEJA-38` N4 case/NFC flow path spelling collisions.
pub fn parse_walk_manifest(contents: &str) -> Result<WalkManifest, CoreError> {
    let parsed: Value = serde_json::from_str(contents)
        .map_err(|_| CoreError::InvalidManifest("not valid JSON".to_string()))?;

    if let Some(repeated) = find_duplicate_json_key(contents) {
        return Err(CoreError::InvalidManifest(format!(
            "the field \"{repeated}\" appears twice in one object"
        )));
    }

    let Some(root) = parsed.as_object() else {
        return Err(CoreError::InvalidManifest("expected an object".to_string()));
    };

    if root
        .get("manifestVersion")
        .and_then(json_non_negative_integer)
        != Some(u64::from(WALK_MANIFEST_VERSION))
    {
        return Err(CoreError::InvalidManifest(format!(
            "unsupported manifestVersion: {}",
            root.get("manifestVersion")
                .map_or_else(|| "undefined".to_string(), Value::to_string)
        )));
    }
    if root.get("hashVersion").and_then(json_non_negative_integer)
        != Some(u64::from(MANIFEST_HASH_VERSION))
    {
        return Err(CoreError::InvalidManifest(format!(
            "unsupported hashVersion: {}",
            root.get("hashVersion")
                .map_or_else(|| "undefined".to_string(), Value::to_string)
        )));
    }
    only_keys(
        root,
        &["manifestVersion", "hashVersion", "walks"],
        "manifest",
    )?;

    let Some(raw_walks) = root.get("walks").and_then(Value::as_array) else {
        return Err(CoreError::InvalidManifest(
            "walks must be an array".to_string(),
        ));
    };

    let mut walks = Vec::with_capacity(raw_walks.len());
    for (index, item) in raw_walks.iter().enumerate() {
        walks.push(read_walk(item, index)?);
    }

    let mut seen_identities = HashSet::with_capacity(walks.len());
    let mut flow_spellings: HashMap<String, String> = HashMap::with_capacity(walks.len());

    for walk in &walks {
        let key = manifest_walk_key(walk)?;
        if !seen_identities.insert(key) {
            return Err(CoreError::InvalidManifest(format!(
                "duplicate entry for goal \"{}\"",
                walk.goal
            )));
        }

        // DEJA-38 N4: reject two distinct flow path spellings that differ only in case or NFC/NFD.
        let canonical_spelling = spelling_key(&walk.flow);
        if let Some(existing_spelling) = flow_spellings.get(&canonical_spelling) {
            if existing_spelling != &walk.flow {
                return Err(CoreError::InvalidManifest(format!(
                    "two flow paths differ only in case or Unicode normalization: {existing_spelling} and {}",
                    walk.flow
                )));
            }
        } else {
            flow_spellings.insert(canonical_spelling, walk.flow.clone());
        }
    }

    Ok(WalkManifest {
        manifest_version: WALK_MANIFEST_VERSION,
        hash_version: MANIFEST_HASH_VERSION,
        walks,
    })
}

fn push_indent(out: &mut String, spaces: usize) {
    for _ in 0..spaces {
        out.push(' ');
    }
}

fn write_json_string(out: &mut String, text: &str) {
    out.push_str(&serde_json::to_string(text).expect("UTF-8 string serializes"));
}

fn write_string_array(out: &mut String, items: &[String], indent: usize) {
    if items.is_empty() {
        out.push_str("[]");
        return;
    }
    out.push_str("[\n");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        push_indent(out, indent + 2);
        write_json_string(out, item);
    }
    out.push('\n');
    push_indent(out, indent);
    out.push(']');
}

fn write_owner_rules(out: &mut String, rules: &ManifestOwnerRules, indent: usize) {
    let mut fields: Vec<(&str, String)> = Vec::new();
    if let Some(min) = rules.min_assertions {
        fields.push(("minAssertions", min.to_string()));
    }
    if let Some(ref ids) = rules.required_assertion_ids {
        let mut buf = String::new();
        write_string_array(&mut buf, ids, indent + 2);
        fields.push(("requiredAssertionIds", buf));
    }
    if let Some(disallow) = rules.disallow_text_selectors {
        fields.push(("disallowTextSelectors", disallow.to_string()));
    }
    if let Some(ref allowed) = rules.allowed_text_selectors {
        let mut buf = String::new();
        write_string_array(&mut buf, allowed, indent + 2);
        fields.push(("allowedTextSelectors", buf));
    }

    if fields.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push_str("{\n");
    for (index, (key, rendered)) in fields.into_iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        push_indent(out, indent + 2);
        write_json_string(out, key);
        out.push_str(": ");
        out.push_str(&rendered);
    }
    out.push('\n');
    push_indent(out, indent);
    out.push('}');
}

fn write_profile(out: &mut String, profile: &ManifestProfile, indent: usize) {
    let mut fields: Vec<(&str, String)> = Vec::new();
    if let Some(ref name) = profile.name {
        fields.push((
            "name",
            serde_json::to_string(name).expect("UTF-8 string serializes"),
        ));
    }
    if let Some(ref launch_args) = profile.launch_arguments {
        let mut sorted: Vec<(&String, &LaunchArgumentValue)> = launch_args.iter().collect();
        // Match JS `Object.fromEntries(Object.entries(...).sort(...))` property enumeration order:
        // canonical 32-bit array indices ascending numerically first, then UTF-16 sorted keys.
        sorted.sort_by(|(left, _), (right, _)| {
            match (js_array_index(left), js_array_index(right)) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => cmp_utf16(left, right),
            }
        });
        let mut buf = String::new();
        if sorted.is_empty() {
            buf.push_str("{}");
        } else {
            buf.push_str("{\n");
            for (index, (k, v)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    buf.push_str(",\n");
                }
                push_indent(&mut buf, indent + 4);
                write_json_string(&mut buf, k);
                buf.push_str(": ");
                let rendered_val = match v {
                    LaunchArgumentValue::String(s) => {
                        serde_json::to_string(s).expect("string launch arg serializes")
                    }
                    LaunchArgumentValue::Bool(b) => b.to_string(),
                    LaunchArgumentValue::Number(n) => {
                        if let Some(f) = n.as_f64() {
                            js_json_number_to_string(f)
                        } else {
                            n.to_string()
                        }
                    }
                };
                buf.push_str(&rendered_val);
            }
            buf.push('\n');
            push_indent(&mut buf, indent + 2);
            buf.push('}');
        }
        fields.push(("launchArguments", buf));
    }
    if let Some(clear_state) = profile.clear_state {
        fields.push(("clearState", clear_state.to_string()));
    }

    if fields.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push_str("{\n");
    for (index, (key, rendered)) in fields.into_iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        push_indent(out, indent + 2);
        write_json_string(out, key);
        out.push_str(": ");
        out.push_str(&rendered);
    }
    out.push('\n');
    push_indent(out, indent);
    out.push('}');
}

fn write_manifest_walk(out: &mut String, walk: &ManifestWalk, indent: usize) {
    let mut fields: Vec<(&str, String)> = Vec::with_capacity(10);
    fields.push(("goal", serde_json::to_string(&walk.goal).expect("goal")));
    fields.push(("appId", serde_json::to_string(&walk.app_id).expect("appId")));
    fields.push((
        "platform",
        serde_json::to_string(&walk.platform).expect("platform"),
    ));
    fields.push((
        "adapter",
        serde_json::to_string(&walk.adapter).expect("adapter"),
    ));
    if let Some(ref browser) = walk.browser {
        fields.push(("browser", serde_json::to_string(browser).expect("browser")));
    }
    fields.push(("flow", serde_json::to_string(&walk.flow).expect("flow")));
    fields.push((
        "closureSha256",
        serde_json::to_string(&walk.closure_sha256).expect("closureSha256"),
    ));
    fields.push((
        "assertionsSha256",
        serde_json::to_string(&walk.assertions_sha256).expect("assertionsSha256"),
    ));
    if let Some(ref owner_rules) = walk.owner_rules {
        let mut buf = String::new();
        write_owner_rules(&mut buf, owner_rules, indent + 2);
        fields.push(("ownerRules", buf));
    }
    if let Some(ref profile) = walk.profile {
        let mut buf = String::new();
        write_profile(&mut buf, profile, indent + 2);
        fields.push(("profile", buf));
    }

    out.push_str("{\n");
    for (index, (key, rendered)) in fields.into_iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        push_indent(out, indent + 2);
        write_json_string(out, key);
        out.push_str(": ");
        out.push_str(&rendered);
    }
    out.push('\n');
    push_indent(out, indent);
    out.push('}');
}

/// Deterministic serialization matching TypeScript `serializeWalkManifest` (fixed key order,
/// sorted walk entries by UTF-16 `manifestWalkKey`, 2-space indent, single trailing newline).
pub fn serialize_walk_manifest(manifest: &WalkManifest) -> Result<String, CoreError> {
    let mut keyed: Vec<(String, &ManifestWalk)> = Vec::with_capacity(manifest.walks.len());
    for walk in &manifest.walks {
        keyed.push((manifest_walk_key(walk)?, walk));
    }
    keyed.sort_by(|left, right| cmp_utf16(&left.0, &right.0));

    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"manifestVersion\": {WALK_MANIFEST_VERSION},\n"
    ));
    out.push_str(&format!("  \"hashVersion\": {MANIFEST_HASH_VERSION},\n"));
    if keyed.is_empty() {
        out.push_str("  \"walks\": []\n");
    } else {
        out.push_str("  \"walks\": [\n");
        for (index, (_, walk)) in keyed.into_iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            push_indent(&mut out, 4);
            write_manifest_walk(&mut out, walk, 4);
        }
        out.push_str("\n  ]\n");
    }
    out.push_str("}\n");
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Owner rules & DEJA-57 disallowTextSelectors lint

const DIRECT_SELECTOR_COMMANDS: &[&str] = &[
    "tapOn",
    "doubleTapOn",
    "longPressOn",
    "assertVisible",
    "assertNotVisible",
    "copyTextFrom",
];

const RELATIVE_SELECTOR_FIELDS: &[&str] = &[
    "childOf",
    "below",
    "above",
    "leftOf",
    "rightOf",
    "containsChild",
    "containsDescendants",
];

fn has_non_empty_id(value: &YamlValue) -> bool {
    value
        .get("id")
        .and_then(YamlValue::as_str)
        .is_some_and(|id| !trim_es_whitespace(id).is_empty())
}

fn check_selector_node(
    file: &str,
    command: &str,
    node: &YamlValue,
    allowed: &HashSet<&str>,
    violations: &mut Vec<TextSelectorViolation>,
) {
    match node {
        YamlValue::String(text) => {
            if !allowed.contains(text.as_str()) {
                violations.push(TextSelectorViolation {
                    file: file.to_string(),
                    command: command.to_string(),
                    selector: text.clone(),
                });
            }
        }
        YamlValue::Map(entries) => {
            if !has_non_empty_id(node)
                && let Some(text) = node.get("text").and_then(YamlValue::as_str)
                && !allowed.contains(text)
            {
                violations.push(TextSelectorViolation {
                    file: file.to_string(),
                    command: command.to_string(),
                    selector: text.to_string(),
                });
            }
            for (field, sub) in entries {
                if RELATIVE_SELECTOR_FIELDS.contains(&field.as_str()) {
                    if field == "containsDescendants" {
                        if let Some(items) = sub.as_seq() {
                            for item in items {
                                check_selector_node(file, command, item, allowed, violations);
                            }
                        }
                    } else {
                        check_selector_node(file, command, sub, allowed, violations);
                    }
                }
            }
        }
        YamlValue::Null | YamlValue::Bool(_) | YamlValue::Number(_) | YamlValue::Seq(_) => {}
    }
}

fn check_condition_selectors(
    file: &str,
    command: &str,
    condition: &YamlValue,
    allowed: &HashSet<&str>,
    violations: &mut Vec<TextSelectorViolation>,
) {
    if let Some(vis) = condition.get("visible") {
        check_selector_node(file, command, vis, allowed, violations);
    }
    if let Some(not_vis) = condition.get("notVisible") {
        check_selector_node(file, command, not_vis, allowed, violations);
    }
}

fn scan_commands_for_text_selectors(
    file: &str,
    node: &YamlValue,
    allowed: &HashSet<&str>,
    violations: &mut Vec<TextSelectorViolation>,
) {
    match node {
        YamlValue::Seq(items) => {
            for item in items {
                scan_commands_for_text_selectors(file, item, allowed, violations);
            }
        }
        YamlValue::Map(entries) => {
            for (key, value) in entries {
                if DIRECT_SELECTOR_COMMANDS.contains(&key.as_str()) {
                    check_selector_node(file, key, value, allowed, violations);
                    continue;
                }
                match key.as_str() {
                    "extendedWaitUntil" => {
                        check_condition_selectors(
                            file,
                            "extendedWaitUntil",
                            value,
                            allowed,
                            violations,
                        );
                    }
                    "scrollUntilVisible" => {
                        if let Some(element) = value.get("element") {
                            check_selector_node(
                                file,
                                "scrollUntilVisible",
                                element,
                                allowed,
                                violations,
                            );
                        }
                    }
                    "swipe" => {
                        if let Some(from) = value.get("from") {
                            check_selector_node(file, "swipe.from", from, allowed, violations);
                        }
                    }
                    "repeat" => {
                        if let Some(cond) = value.get("while") {
                            check_condition_selectors(
                                file,
                                "repeat.while",
                                cond,
                                allowed,
                                violations,
                            );
                        }
                        if let Some(commands) = value.get("commands") {
                            scan_commands_for_text_selectors(file, commands, allowed, violations);
                        }
                    }
                    "runFlow" => {
                        if let Some(cond) = value.get("when") {
                            check_condition_selectors(
                                file,
                                "runFlow.when",
                                cond,
                                allowed,
                                violations,
                            );
                        }
                        if let Some(commands) = value.get("commands") {
                            scan_commands_for_text_selectors(file, commands, allowed, violations);
                        }
                    }
                    "retry" => {
                        if let Some(cond) = value.get("when") {
                            check_condition_selectors(
                                file,
                                "retry.when",
                                cond,
                                allowed,
                                violations,
                            );
                        }
                        if let Some(commands) = value.get("commands") {
                            scan_commands_for_text_selectors(file, commands, allowed, violations);
                        }
                    }
                    "runScript" => {
                        if let Some(cond) = value.get("when") {
                            check_condition_selectors(
                                file,
                                "runScript.when",
                                cond,
                                allowed,
                                violations,
                            );
                        }
                    }
                    "onFlowStart" | "onFlowComplete" => {
                        scan_commands_for_text_selectors(file, value, allowed, violations);
                    }
                    _ => {}
                }
            }
        }
        YamlValue::Null | YamlValue::Bool(_) | YamlValue::Number(_) | YamlValue::String(_) => {}
    }
}

/// Find every text-only UI selector across all YAML flows in a [`FlowClosure`] (`DEJA-57`).
///
/// Selectors anchored by a non-empty `id` (even when paired with `text`) are permitted; bare string
/// selectors and `{ text: ... }` without `id` are flagged unless listed in `allowed_text_selectors`.
/// Non-selector commands such as `assertTrue` and `inputText` are never flagged.
pub fn find_disallowed_text_selectors(
    closure: &FlowClosure,
    allowed_text_selectors: &[String],
) -> Result<Vec<TextSelectorViolation>, CoreError> {
    let allowed: HashSet<&str> = allowed_text_selectors.iter().map(String::as_str).collect();
    let mut violations = Vec::new();

    for file in &closure.files {
        if file.kind != FlowReferenceKind::Flow {
            continue;
        }
        let text = String::from_utf8_lossy(&file.bytes);
        let documents =
            parse_yaml_documents(&text).map_err(|err| CoreError::FlowReference(err.to_string()))?;
        for doc in &documents {
            scan_commands_for_text_selectors(&file.path, doc, &allowed, &mut violations);
        }
    }

    Ok(violations)
}

/// Enforce all configured [`ManifestOwnerRules`] against expanded closure assertions and closure files.
pub fn check_owner_rules(
    rules: Option<&ManifestOwnerRules>,
    assertions: &[ClosureAssertion],
    closure: Option<&FlowClosure>,
) -> Result<(), CoreError> {
    let Some(rules) = rules else {
        return Ok(());
    };

    let asserts: Vec<&ClosureAssertion> = assertions
        .iter()
        .filter(|assertion| assertion.command.starts_with("assert"))
        .collect();

    if let Some(min) = rules.min_assertions
        && (asserts.len() as u64) < min
    {
        return Err(CoreError::OwnerRule(format!(
            "the flow has {} assertions, the manifest requires at least {min}",
            asserts.len()
        )));
    }

    if let Some(ref required_ids) = rules.required_assertion_ids {
        let visible_ids: HashSet<&str> = asserts
            .iter()
            .filter(|item| item.command == "assertVisible" && item.always && item.reached_always)
            .filter_map(|item| item.id.as_deref())
            .collect();
        for id in required_ids {
            if !visible_ids.contains(id.as_str()) {
                return Err(CoreError::OwnerRule(format!(
                    "the flow never runs assertVisible on id \"{id}\" unconditionally"
                )));
            }
        }
    }

    if rules.disallow_text_selectors == Some(true) {
        let Some(closure) = closure else {
            return Err(CoreError::OwnerRule(
                "disallowTextSelectors requires flow closure files to inspect".to_string(),
            ));
        };
        let allowed = rules.allowed_text_selectors.as_deref().unwrap_or(&[]);
        let violations = find_disallowed_text_selectors(closure, allowed)?;
        if !violations.is_empty() {
            let summary = violations
                .iter()
                .map(|v| format!("{}: {} \"{}\"", v.file, v.command, v.selector))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(CoreError::OwnerRule(format!(
                "text-only selectors are disallowed by ownerRules.disallowTextSelectors: {summary}"
            )));
        }
    }

    Ok(())
}
