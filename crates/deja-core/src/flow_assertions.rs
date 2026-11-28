//! Assertion extraction, closure-level expansion with DEJA-38 memoization and line cap,
//! and version-1 `assertionsSha256` hashing.

use std::collections::HashMap;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canonical_json::canonical_json;
use crate::contracts::trim_es_whitespace;
use crate::error::CoreError;
use crate::flow_closure::{ClosureFile, FlowClosure, MAX_DEPTH};
use crate::yaml::{YamlValue, parse_yaml_documents};

/// Version of the assertion hash format (`deja-assertions-v1`).
pub const ASSERTION_HASH_VERSION: u32 = 1;

/// Upper bound on expanded assertion lines across a flow closure (`DEJA-38` L2 cap).
pub const MAX_CLOSURE_ASSERTIONS: usize = 50_000;

/// Upper bound on total expanded assertion entry bytes across a flow closure (`DEJA-38` L2 byte cap).
pub const MAX_CLOSURE_ASSERTION_BYTES: usize = 16 * 1024 * 1024;

/// Kind of external file called by a flow command in an assertion trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlowAssertionCallKind {
    Flow,
    Script,
}

/// External call target recorded on a `runFlow`, `retry`, or `runScript` assertion line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowAssertionCall {
    pub kind: FlowAssertionCallKind,
    pub path: String,
}

/// One assertion or control-flow guard line extracted from a single YAML file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowAssertion {
    pub entry: String,
    pub command: String,
    pub id: Option<String>,
    pub always: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<FlowAssertionCall>,
}

/// One assertion line expanded across a [`FlowClosure`] with file path, call chain, and reachability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosureAssertion {
    pub entry: String,
    pub command: String,
    pub id: Option<String>,
    pub always: bool,
    pub reached_always: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<FlowAssertionCall>,
}

fn parameters(value: &YamlValue) -> YamlValue {
    match value {
        YamlValue::Map(entries) => {
            let kept: Vec<(String, YamlValue)> = entries
                .iter()
                .filter(|(k, _)| !matches!(k.as_str(), "commands" | "file" | "label"))
                .cloned()
                .collect();
            YamlValue::Map(kept)
        }
        _ => YamlValue::Map(Vec::new()),
    }
}

/// Anything other than absent or literal `false` counts as optional (fail closed).
fn is_optional(value: &YamlValue) -> bool {
    value
        .get("optional")
        .is_some_and(|v| !matches!(v, YamlValue::Bool(false)))
}

fn is_call_command(key: &str) -> bool {
    matches!(key, "runFlow" | "repeat" | "retry" | "runScript")
}

fn ancestors_always_run(ancestors: &[String]) -> bool {
    ancestors
        .iter()
        .all(|element| element == "onFlowStart" || element == "onFlowComplete")
}

struct FlowAssertionCollector {
    found: Vec<FlowAssertion>,
    total_bytes: usize,
    ancestors: Vec<String>,
    ancestors_len: usize,
}

impl FlowAssertionCollector {
    fn new() -> Self {
        Self {
            found: Vec::new(),
            total_bytes: 0,
            ancestors: Vec::new(),
            ancestors_len: 0,
        }
    }

    fn push_ancestor(&mut self, segment: String) -> Result<(), CoreError> {
        let added = if self.ancestors.is_empty() {
            segment.len()
        } else {
            segment.len().saturating_add(1)
        };
        let next_len = self.ancestors_len.saturating_add(added);
        if next_len > MAX_CLOSURE_ASSERTION_BYTES {
            return Err(CoreError::FlowClosure(format!(
                "closure assertion output exceeds limit of {MAX_CLOSURE_ASSERTION_BYTES} bytes"
            )));
        }
        self.ancestors.push(segment);
        self.ancestors_len = next_len;
        Ok(())
    }

    fn pop_ancestor(&mut self) {
        if let Some(removed) = self.ancestors.pop() {
            let sub = if self.ancestors.is_empty() {
                removed.len()
            } else {
                removed.len().saturating_add(1)
            };
            self.ancestors_len = self.ancestors_len.saturating_sub(sub);
        }
    }

    fn reserve_entry(&mut self, key_len: usize, tail_len: usize) -> Result<(), CoreError> {
        if self.found.len() >= MAX_CLOSURE_ASSERTIONS {
            return Err(CoreError::FlowClosure(format!(
                "closure produces more than {MAX_CLOSURE_ASSERTIONS} assertion entries"
            )));
        }
        let entry_len = self
            .ancestors_len
            .saturating_add(1)
            .saturating_add(key_len)
            .saturating_add(1)
            .saturating_add(tail_len);
        let next_bytes = self.total_bytes.saturating_add(entry_len);
        if next_bytes > MAX_CLOSURE_ASSERTION_BYTES {
            return Err(CoreError::FlowClosure(format!(
                "closure assertion output exceeds limit of {MAX_CLOSURE_ASSERTION_BYTES} bytes"
            )));
        }
        self.total_bytes = next_bytes;
        Ok(())
    }

    fn visit(&mut self, node: &YamlValue, direct: bool) -> Result<(), CoreError> {
        match node {
            YamlValue::Seq(items) => {
                for item in items {
                    if let YamlValue::String(text) = item
                        && text.starts_with("assert")
                    {
                        self.reserve_entry(text.len(), 4)?;
                        let where_str = self.ancestors.join(">");
                        let always = ancestors_always_run(&self.ancestors);
                        self.found.push(FlowAssertion {
                            entry: format!("{where_str}|{text}|null"),
                            command: text.clone(),
                            id: None,
                            always,
                            call: None,
                        });
                    } else {
                        self.visit(item, false)?;
                    }
                }
                Ok(())
            }
            YamlValue::Map(entries) => {
                let always = ancestors_always_run(&self.ancestors);
                for (key, value) in entries {
                    if key.starts_with("assert") {
                        let optional = is_optional(value);
                        let id = value
                            .get("id")
                            .and_then(YamlValue::as_str)
                            .map(str::to_string);
                        let payload = canonical_json(value);
                        self.reserve_entry(key.len(), payload.len())?;
                        let where_str = self.ancestors.join(">");
                        self.found.push(FlowAssertion {
                            entry: format!("{where_str}|{key}|{payload}"),
                            command: key.clone(),
                            id,
                            always: always && !optional,
                            call: None,
                        });
                        continue;
                    }
                    if key == "<<" {
                        let payload = canonical_json(value);
                        self.reserve_entry(2, payload.len())?;
                        let where_str = self.ancestors.join(">");
                        self.found.push(FlowAssertion {
                            entry: format!("{where_str}|<<|{payload}"),
                            command: "<<".to_string(),
                            id: None,
                            always: false,
                            call: None,
                        });
                        continue;
                    }
                    if key == "extendedWaitUntil" {
                        let payload = canonical_json(value);
                        self.reserve_entry(key.len(), payload.len())?;
                        let where_str = self.ancestors.join(">");
                        self.found.push(FlowAssertion {
                            entry: format!("{where_str}|extendedWaitUntil|{payload}"),
                            command: "extendedWaitUntil".to_string(),
                            id: None,
                            always: always && !is_optional(value),
                            call: None,
                        });
                        continue;
                    }
                    if key == "evalScript" {
                        let payload = canonical_json(value);
                        self.reserve_entry(key.len(), payload.len())?;
                        let where_str = self.ancestors.join(">");
                        self.found.push(FlowAssertion {
                            entry: format!("{where_str}|evalScript|{payload}"),
                            command: "evalScript".to_string(),
                            id: None,
                            always,
                            call: None,
                        });
                        continue;
                    }
                    if key == "env" {
                        if !direct {
                            let payload = canonical_json(value);
                            self.reserve_entry(key.len(), payload.len())?;
                            let where_str = self.ancestors.join(">");
                            self.found.push(FlowAssertion {
                                entry: format!("{where_str}|env|{payload}"),
                                command: "env".to_string(),
                                id: None,
                                always,
                                call: None,
                            });
                        }
                        continue;
                    }
                    if is_call_command(key) {
                        let params = canonical_json(&parameters(value));
                        let file = match value {
                            YamlValue::String(s) => Some(s.as_str()),
                            YamlValue::Map(_) => value.get("file").and_then(YamlValue::as_str),
                            _ => None,
                        };
                        let conditional = value.as_map().is_some()
                            && (value.get("when").is_some() || is_optional(value));
                        let call = match file {
                            Some(raw_path) if key != "repeat" => Some(FlowAssertionCall {
                                kind: if key == "runScript" {
                                    FlowAssertionCallKind::Script
                                } else {
                                    FlowAssertionCallKind::Flow
                                },
                                path: trim_es_whitespace(raw_path).to_string(),
                            }),
                            _ => None,
                        };
                        self.reserve_entry(
                            key.len(),
                            "params=".len().saturating_add(params.len()),
                        )?;
                        let where_str = self.ancestors.join(">");
                        self.found.push(FlowAssertion {
                            entry: format!("{where_str}|{key}|params={params}"),
                            command: key.clone(),
                            id: None,
                            always: always && !conditional,
                            call,
                        });
                        if value.as_map().is_some() {
                            self.push_ancestor(format!("{key}{params}"))?;
                            let res = self.visit(value, true);
                            self.pop_ancestor();
                            res?;
                        }
                        continue;
                    }
                    self.push_ancestor(key.clone())?;
                    let res = self.visit(value, false);
                    self.pop_ancestor();
                    res?;
                }
                Ok(())
            }
            YamlValue::Null | YamlValue::Bool(_) | YamlValue::Number(_) | YamlValue::String(_) => {
                Ok(())
            }
        }
    }
}

/// Extract version-1 assertion and control-flow guard lines from a single Maestro YAML file.
///
/// Enforces [`MAX_CLOSURE_ASSERTIONS`] and [`MAX_CLOSURE_ASSERTION_BYTES`] before allocating
/// expanded entries so adversarial inline flows fail closed without exhausting memory.
pub fn find_flow_assertions(contents: &str) -> Result<Vec<FlowAssertion>, CoreError> {
    let documents =
        parse_yaml_documents(contents).map_err(|err| CoreError::FlowReference(err.to_string()))?;
    let mut collector = FlowAssertionCollector::new();
    for doc in &documents {
        collector.visit(doc, false)?;
    }
    Ok(collector.found)
}

fn posix_resolve_relative(from_file: &str, rel: &str) -> String {
    let mut stack: Vec<&str> = from_file.split('/').collect();
    // Drop the filename to keep only its directory segments.
    if !stack.is_empty() {
        stack.pop();
    }
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
}

struct ClosureAssertionWalker<'a> {
    files: HashMap<&'a str, &'a ClosureFile>,
    parsed_cache: HashMap<&'a str, Rc<Vec<FlowAssertion>>>,
    found: Vec<ClosureAssertion>,
    total_bytes: usize,
}

impl<'a> ClosureAssertionWalker<'a> {
    fn check_next_line_budget(&mut self, added_bytes: usize) -> Result<(), CoreError> {
        if self.found.len() >= MAX_CLOSURE_ASSERTIONS {
            return Err(CoreError::FlowClosure(format!(
                "closure produces more than {MAX_CLOSURE_ASSERTIONS} assertion entries"
            )));
        }
        let next_bytes = self.total_bytes.saturating_add(added_bytes);
        if next_bytes > MAX_CLOSURE_ASSERTION_BYTES {
            return Err(CoreError::FlowClosure(format!(
                "closure assertion output exceeds limit of {MAX_CLOSURE_ASSERTION_BYTES} bytes"
            )));
        }
        self.total_bytes = next_bytes;
        Ok(())
    }

    fn assertions_for_file(&mut self, path: &'a str) -> Result<Rc<Vec<FlowAssertion>>, CoreError> {
        if let Some(cached) = self.parsed_cache.get(path) {
            return Ok(Rc::clone(cached));
        }
        let file = self
            .files
            .get(path)
            .ok_or_else(|| CoreError::FlowClosure(format!("closure is missing file: {path}")))?;
        let text = String::from_utf8_lossy(&file.bytes);
        let parsed = Rc::new(find_flow_assertions(&text)?);
        self.parsed_cache.insert(path, Rc::clone(&parsed));
        Ok(parsed)
    }

    fn walk(
        &mut self,
        path: &'a str,
        chain: &mut Vec<String>,
        reached_always: bool,
    ) -> Result<(), CoreError> {
        if chain.len() > MAX_DEPTH {
            return Err(CoreError::FlowClosure(format!(
                "flows are nested more than {MAX_DEPTH} deep"
            )));
        }
        let items = self.assertions_for_file(path)?;
        let chain_prefix_len = if chain.is_empty() {
            0
        } else {
            chain
                .iter()
                .map(String::len)
                .fold(0_usize, usize::saturating_add)
                .saturating_add((chain.len() - 1).saturating_mul(3))
        };
        if chain_prefix_len > MAX_CLOSURE_ASSERTION_BYTES {
            return Err(CoreError::FlowClosure(format!(
                "closure assertion output exceeds limit of {MAX_CLOSURE_ASSERTION_BYTES} bytes"
            )));
        }
        let chain_prefix = chain.join(" > ");

        for item in items.iter() {
            let line_len = path
                .len()
                .saturating_add(1)
                .saturating_add(chain_prefix_len)
                .saturating_add(1)
                .saturating_add(item.entry.len());
            self.check_next_line_budget(line_len)?;

            let line = format!("{path}|{chain_prefix}|{}", item.entry);
            let item_always = item.always;
            let call = item.call.clone();
            self.found.push(ClosureAssertion {
                entry: line.clone(),
                command: item.command.clone(),
                id: item.id.clone(),
                always: item_always,
                reached_always,
                call: call.clone(),
            });

            let Some(call) = call else {
                continue;
            };
            let target_owned = posix_resolve_relative(path, &call.path);
            let Some((&target_key, &reached)) = self.files.get_key_value(target_owned.as_str())
            else {
                return Err(CoreError::FlowClosure(format!(
                    "{path} runs {}, which the closure does not hold",
                    call.path
                )));
            };

            match call.kind {
                FlowAssertionCallKind::Script => {
                    let script_line_len = line
                        .len()
                        .saturating_add("|script=".len())
                        .saturating_add(target_key.len())
                        .saturating_add("|sha256=".len())
                        .saturating_add(reached.sha256.len());
                    self.check_next_line_budget(script_line_len)?;
                    self.found.push(ClosureAssertion {
                        entry: format!("{line}|script={target_key}|sha256={}", reached.sha256),
                        command: "runScript".to_string(),
                        id: None,
                        always: item_always,
                        reached_always,
                        call: None,
                    });
                }
                FlowAssertionCallKind::Flow => {
                    chain.push(line);
                    let res = self.walk(target_key, chain, reached_always && item_always);
                    chain.pop();
                    res?;
                }
            }
        }
        Ok(())
    }
}

/// Expand all assertions of a [`FlowClosure`] in execution order, memoizing per-file YAML parses
/// and capping total lines at [`MAX_CLOSURE_ASSERTIONS`] and total bytes at
/// [`MAX_CLOSURE_ASSERTION_BYTES`] (`DEJA-38` L2).
pub fn closure_assertions(closure: &FlowClosure) -> Result<Vec<ClosureAssertion>, CoreError> {
    let files: HashMap<&str, &ClosureFile> = closure
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let main_key = files
        .get_key_value(closure.main.as_str())
        .map(|(&k, _)| k)
        .ok_or_else(|| {
            CoreError::FlowClosure(format!("closure is missing main flow: {}", closure.main))
        })?;

    let mut walker = ClosureAssertionWalker {
        files,
        parsed_cache: HashMap::new(),
        found: Vec::new(),
        total_bytes: 0,
    };
    let mut chain = Vec::new();
    walker.walk(main_key, &mut chain, true)?;
    Ok(walker.found)
}

/// Compute the version-1 `assertionsSha256` digest over a [`FlowClosure`].
pub fn assertions_sha256(closure: &FlowClosure) -> Result<String, CoreError> {
    let assertions = closure_assertions(closure)?;
    let lines: Vec<&str> = assertions.iter().map(|a| a.entry.as_str()).collect();
    let payload = format!(
        "deja-assertions-v{ASSERTION_HASH_VERSION}\n{}\n",
        lines.join("\n")
    );
    Ok(hex::encode(Sha256::digest(payload.as_bytes())))
}
