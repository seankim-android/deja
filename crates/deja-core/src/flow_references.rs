//! Discover every external file referenced by a Maestro YAML flow (`runFlow`, `retry`, `runScript`, `addMedia`).

use serde::{Deserialize, Serialize};

use crate::contracts::trim_es_whitespace;
use crate::error::CoreError;
use crate::yaml::{YamlValue, parse_yaml_documents};

/// Kind of external file referenced by a Maestro flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlowReferenceKind {
    Flow,
    Script,
    Media,
}

impl FlowReferenceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flow => "flow",
            Self::Script => "script",
            Self::Media => "media",
        }
    }
}

/// Maestro command that named a referenced file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FlowReferenceCommand {
    RunFlow,
    Retry,
    RunScript,
    AddMedia,
}

/// A file reference discovered in a Maestro YAML flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowReference {
    pub kind: FlowReferenceKind,
    pub command: FlowReferenceCommand,
    pub path: String,
}

const UNSUPPORTED_FILE_COMMANDS: &[&str] = &["assertScreenshot"];

fn reference_path(kind: &str, value: Option<&YamlValue>) -> Result<String, CoreError> {
    match value {
        Some(YamlValue::String(s)) => {
            let trimmed = trim_es_whitespace(s);
            if trimmed.is_empty() {
                Err(CoreError::FlowReference(format!(
                    "{kind} needs a file path"
                )))
            } else {
                Ok(trimmed.to_string())
            }
        }
        _ => Err(CoreError::FlowReference(format!(
            "{kind} needs a file path"
        ))),
    }
}

fn media_paths(value: &YamlValue) -> Result<Vec<String>, CoreError> {
    let files = match value {
        YamlValue::Map(_) => value.get("files"),
        other => Some(other),
    };
    let Some(YamlValue::Seq(items)) = files else {
        return Err(CoreError::FlowFileCommand(
            "addMedia needs a list of file paths".to_string(),
        ));
    };
    if items.is_empty() {
        return Err(CoreError::FlowFileCommand(
            "addMedia needs a list of file paths".to_string(),
        ));
    }
    let mut paths = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(CoreError::FlowFileCommand(
                "addMedia needs a list of file paths".to_string(),
            ));
        };
        let trimmed = trim_es_whitespace(text);
        if trimmed.is_empty() {
            return Err(CoreError::FlowFileCommand(
                "addMedia needs a list of file paths".to_string(),
            ));
        }
        paths.push(trimmed.to_string());
    }
    Ok(paths)
}

fn visit_references(node: &YamlValue, found: &mut Vec<FlowReference>) -> Result<(), CoreError> {
    match node {
        YamlValue::Seq(items) => {
            for item in items {
                visit_references(item, found)?;
            }
            Ok(())
        }
        YamlValue::Map(entries) => {
            for (key, value) in entries {
                if UNSUPPORTED_FILE_COMMANDS.contains(&key.as_str()) {
                    return Err(CoreError::FlowFileCommand(format!(
                        "{key} reads a reference image that Déjà does not store or hash, so a stored or manifest flow cannot use it"
                    )));
                }
                if key == "addMedia" {
                    for path in media_paths(value)? {
                        found.push(FlowReference {
                            kind: FlowReferenceKind::Media,
                            command: FlowReferenceCommand::AddMedia,
                            path,
                        });
                    }
                    continue;
                }
                if matches!(key.as_str(), "runFlow" | "runScript" | "retry") {
                    let (kind, command) = match key.as_str() {
                        "runScript" => (FlowReferenceKind::Script, FlowReferenceCommand::RunScript),
                        "retry" => (FlowReferenceKind::Flow, FlowReferenceCommand::Retry),
                        _ => (FlowReferenceKind::Flow, FlowReferenceCommand::RunFlow),
                    };
                    if let YamlValue::Map(_) = value {
                        let file = value.get("file");
                        if file.is_some() || key == "runScript" {
                            found.push(FlowReference {
                                kind,
                                command,
                                path: reference_path(key, file)?,
                            });
                        }
                    } else if key == "retry" {
                        return Err(CoreError::FlowFileCommand(
                            "retry takes a mapping with a file or commands, not a bare value"
                                .to_string(),
                        ));
                    } else {
                        found.push(FlowReference {
                            kind,
                            command,
                            path: reference_path(key, Some(value))?,
                        });
                    }
                }
                visit_references(value, found)?;
            }
            Ok(())
        }
        YamlValue::Null | YamlValue::Bool(_) | YamlValue::Number(_) | YamlValue::String(_) => {
            Ok(())
        }
    }
}

/// Find every file a Maestro flow reads across all YAML documents and nested blocks.
pub fn find_flow_references(contents: &str) -> Result<Vec<FlowReference>, CoreError> {
    let documents =
        parse_yaml_documents(contents).map_err(|err| CoreError::FlowReference(err.to_string()))?;
    let mut found = Vec::new();
    for doc in &documents {
        visit_references(doc, &mut found)?;
    }
    Ok(found)
}
