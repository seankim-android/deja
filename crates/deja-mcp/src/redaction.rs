//! Comprehensive path, fixture, JSON-field, environment secret, and provider-key redaction
//! for every `deja-mcp` response, error, report, and diagnostic field.

use std::collections::BTreeMap;
use std::path::Path;

use deja_core::{
    ProviderKeyRedactor, Redactor, StoredWalk, create_redactor, normalize_goal,
    provider_key_redactor, redact_json,
};
use deja_flow::VisibleContext;
use serde_json::Value;

/// Combined path, fixture/secret literal, JSON key-name, and provider-key redactor.
#[derive(Debug, Clone)]
pub struct RedactionContext {
    pub redactor: Redactor,
    pub provider: ProviderKeyRedactor,
}

impl RedactionContext {
    /// Construct a [`RedactionContext`] from workspace root, optional home directory, configured
    /// forbidden secret values, and process environment variables.
    #[must_use]
    pub fn new(
        workspace_root: &Path,
        home_dir: Option<&Path>,
        forbidden_values: &[String],
        env_vars: &BTreeMap<String, String>,
    ) -> Self {
        let ws = workspace_root.to_string_lossy().into_owned();
        let home_owned = home_dir.map(|p| p.to_string_lossy().into_owned());
        let mut expanded_forbidden: Vec<String> = Vec::with_capacity(forbidden_values.len() * 3);
        for v in forbidden_values {
            expanded_forbidden.push(v.clone());
            let lower = v.to_lowercase();
            if lower != *v && lower.chars().count() >= 4 {
                expanded_forbidden.push(lower.clone());
            }
            let slug = slugify_for_redaction(&lower);
            if slug != lower && slug != *v && slug.chars().count() >= 4 {
                expanded_forbidden.push(slug);
            }
        }
        let redactor = create_redactor(&ws, home_owned.as_deref(), &expanded_forbidden);
        let provider =
            provider_key_redactor(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        Self { redactor, provider }
    }

    /// Redact an arbitrary text string through both configured and provider-key redactors.
    #[must_use]
    pub fn redact_str(&self, text: &str) -> String {
        self.provider.redact(&self.redactor.redact(text))
    }

    /// Derive a safe exported `goalKey` by redacting `raw_goal` BEFORE `normalize_goal` lowercasing,
    /// so mixed-case configured secrets never leak in lowercased form through exported `goalKey` fields.
    #[must_use]
    pub fn redact_goal_key(&self, raw_goal: &str) -> String {
        let redacted = self.redact_str(raw_goal);
        normalize_goal(&redacted)
            .map(|g| g.key)
            .unwrap_or_else(|_| redacted.trim().to_ascii_lowercase())
    }

    /// Redact a [`VisibleContext`] while preserving deduplication and truncation markers.
    #[must_use]
    pub fn redact_visible_context(&self, vc: &VisibleContext) -> VisibleContext {
        vc.map_items(|item| self.redact_str(item))
    }

    /// Prepare a [`StoredWalk`] for export with its `goal_key` safely redacted prior to JSON scrubbing.
    #[must_use]
    pub fn sanitize_walk_for_export(&self, walk: &StoredWalk) -> StoredWalk {
        let mut exported = walk.clone();
        exported.goal_key = self.redact_goal_key(&walk.goal);
        exported
    }

    /// Recursively redact a JSON value (secret key names + workspace/home/fixture + provider patterns).
    #[must_use]
    pub fn redact_json(&self, value: &Value) -> Value {
        let first = redact_json(value, &self.redactor);
        redact_json_provider_pass(&first, &self.provider)
    }
}

fn slugify_for_redaction(lower: &str) -> String {
    let mut slug = String::with_capacity(lower.len());
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn redact_json_provider_pass(value: &Value, provider: &ProviderKeyRedactor) -> Value {
    match value {
        Value::String(s) => Value::String(provider.redact(s)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_json_provider_pass(item, provider))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), redact_json_provider_pass(v, provider));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}
