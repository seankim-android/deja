//! `maestro/v1` YAML flow parser built on `deja_core::parse_yaml_documents`.
//!
//! Enforces strict fail-closed rejection of unsupported script/AI/screenshot commands,
//! JavaScript expression conditions (`assertTrue`, `when.true`, `while.true`), and unknown
//! commands so unsupported flows never silently succeed.

use std::collections::BTreeMap;

use deja_core::{
    CoreError, LaunchArgumentValue, YamlValue, js_number_to_string, parse_yaml_documents,
    trim_es_whitespace,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::selector::{PointSpec, Selector, SelectorError};

/// Maximum AST nesting depth for inline control-flow blocks (`runFlow`/`repeat`/`retry`).
const MAX_PARSE_DEPTH: usize = 64;

/// Default character count erased by bare `eraseText` in Maestro.
pub const DEFAULT_ERASE_TEXT_CHARS: u32 = 50;

/// Default timeout for `scrollUntilVisible` in milliseconds.
pub const DEFAULT_SCROLL_TIMEOUT_MS: u64 = 20_000;

/// Default scroll speed (`1..=100`) for `scrollUntilVisible`.
pub const DEFAULT_SCROLL_SPEED: u8 = 40;

/// Default visibility percentage (`1..=100`) for `scrollUntilVisible`.
pub const DEFAULT_VISIBILITY_PERCENTAGE: u8 = 100;

/// Default swipe duration in milliseconds.
pub const DEFAULT_SWIPE_DURATION_MS: u64 = 400;

/// Default timeout for `extendedWaitUntil` in milliseconds.
pub const DEFAULT_EXTENDED_WAIT_TIMEOUT_MS: u64 = 5_000;

/// Commands explicitly rejected by the native Rust interpreter (fail closed).
const EXPLICIT_UNSUPPORTED_COMMANDS: &[(&str, &str)] = &[
    (
        "runScript",
        "JavaScript execution via `runScript` is not supported by the native interpreter",
    ),
    (
        "evalScript",
        "inline JavaScript execution via `evalScript` is not supported by the native interpreter",
    ),
    (
        "assertScreenshot",
        "reference-image comparison via `assertScreenshot` is not supported",
    ),
    (
        "assertNoDefectsWithAi",
        "AI visual defect assertions via `assertNoDefectsWithAi` are not supported",
    ),
    (
        "startRecording",
        "video recording via `startRecording` is not supported",
    ),
    (
        "stopRecording",
        "video recording via `stopRecording` is not supported",
    ),
    (
        "assertTrue",
        "JavaScript expression assertions via `assertTrue` are not supported",
    ),
];

/// Errors emitted while parsing a `maestro/v1` YAML flow.
#[derive(Debug, Error)]
pub enum FlowParseError {
    #[error(transparent)]
    Yaml(#[from] CoreError),

    #[error("flow contains {0} YAML documents (at most 2 are allowed: optional header + commands)")]
    TooManyDocuments(usize),

    #[error("invalid flow structure: {0}")]
    InvalidStructure(String),

    #[error("invalid `{command}` step: {detail}")]
    InvalidStep { command: String, detail: String },

    #[error("unsupported Maestro command `{command}`: {reason}")]
    UnsupportedCommand { command: String, reason: String },

    #[error(transparent)]
    Selector(#[from] SelectorError),
}

/// Scroll or swipe direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ScrollDirection {
    Down,
    Up,
    Left,
    Right,
}

impl ScrollDirection {
    fn parse(raw: &str, command: &str) -> Result<Self, FlowParseError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "DOWN" => Ok(Self::Down),
            "UP" => Ok(Self::Up),
            "LEFT" => Ok(Self::Left),
            "RIGHT" => Ok(Self::Right),
            other => Err(FlowParseError::InvalidStep {
                command: command.to_string(),
                detail: format!("invalid direction `{other}` (expected DOWN, UP, LEFT, or RIGHT)"),
            }),
        }
    }
}

/// Optional YAML front-matter header (document 1 of a 2-document `maestro/v1` flow).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowHeader {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_flow_start: Vec<FlowStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_flow_complete: Vec<FlowStep>,
}

/// Parsed `maestro/v1` flow ready for execution by [`crate::executor::FlowExecutor`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedFlow {
    pub header: FlowHeader,
    pub steps: Vec<FlowStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchAppStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    pub clear_state: bool,
    pub clear_keychain: bool,
    pub stop_app: bool,
    /// `None` when omitted in YAML (preserving platform default permissions, e.g. Maestro's
    /// `all: allow` on iOS); `Some(empty)` when explicitly written as `permissions: {}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub arguments: BTreeMap<String, LaunchArgumentValue>,
}

impl Default for LaunchAppStep {
    fn default() -> Self {
        Self {
            app_id: None,
            clear_state: false,
            clear_keychain: false,
            stop_app: true,
            permissions: None,
            arguments: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppTargetStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenLinkStep {
    pub link: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_verify: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddMediaStep {
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapStep {
    pub selector: Selector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputTextStep {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EraseTextStep {
    pub characters_to_erase: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PressKeyStep {
    pub key: String,
    #[serde(default)]
    pub modifiers: Vec<String>,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollUntilVisibleStep {
    pub element: Selector,
    pub direction: ScrollDirection,
    pub timeout_ms: u64,
    pub speed: u8,
    pub visibility_percentage: u8,
    pub center_element: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwipeStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<ScrollDirection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<PointSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<PointSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Selector>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitForAnimationToEndStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeScreenshotStep {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssertVisibleStep {
    pub selector: Selector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssertNotVisibleStep {
    pub selector: Selector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtendedWaitUntilStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<Selector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_visible: Option<Selector>,
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Declarative guard condition for `runFlow.when` and `repeat.while`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<Selector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_visible: Option<Selector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// Subflow body referenced by `runFlow` or `retry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum RunFlowTarget {
    File(String),
    Commands(Vec<FlowStep>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunFlowStep {
    pub target: RunFlowTarget,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Condition>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepeatStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub while_cond: Option<Condition>,
    pub commands: Vec<FlowStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryStep {
    pub max_retries: u32,
    pub target: RunFlowTarget,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Every supported `maestro/v1` flow step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "params", rename_all = "camelCase")]
pub enum FlowStep {
    // Lifecycle
    LaunchApp(LaunchAppStep),
    StopApp(AppTargetStep),
    KillApp(AppTargetStep),
    ClearState(AppTargetStep),
    ClearKeychain,
    OpenLink(OpenLinkStep),
    AddMedia(AddMediaStep),
    // UI actions
    TapOn(TapStep),
    LongPressOn(TapStep),
    DoubleTapOn(TapStep),
    InputText(InputTextStep),
    EraseText(EraseTextStep),
    PressKey(PressKeyStep),
    HideKeyboard,
    Scroll,
    ScrollUntilVisible(ScrollUntilVisibleStep),
    Swipe(SwipeStep),
    WaitForAnimationToEnd(WaitForAnimationToEndStep),
    TakeScreenshot(TakeScreenshotStep),
    Back,
    // Assertions / waits
    AssertVisible(AssertVisibleStep),
    AssertNotVisible(AssertNotVisibleStep),
    ExtendedWaitUntil(ExtendedWaitUntilStep),
    // Control flow
    RunFlow(RunFlowStep),
    Repeat(RepeatStep),
    Retry(RetryStep),
}

impl FlowStep {
    /// Canonical `maestro/v1` command name for telemetry and classification.
    pub const fn command_name(&self) -> &'static str {
        match self {
            Self::LaunchApp(_) => "launchApp",
            Self::StopApp(_) => "stopApp",
            Self::KillApp(_) => "killApp",
            Self::ClearState(_) => "clearState",
            Self::ClearKeychain => "clearKeychain",
            Self::OpenLink(_) => "openLink",
            Self::AddMedia(_) => "addMedia",
            Self::TapOn(_) => "tapOn",
            Self::LongPressOn(_) => "longPressOn",
            Self::DoubleTapOn(_) => "doubleTapOn",
            Self::InputText(_) => "inputText",
            Self::EraseText(_) => "eraseText",
            Self::PressKey(_) => "pressKey",
            Self::HideKeyboard => "hideKeyboard",
            Self::Scroll => "scroll",
            Self::ScrollUntilVisible(_) => "scrollUntilVisible",
            Self::Swipe(_) => "swipe",
            Self::WaitForAnimationToEnd(_) => "waitForAnimationToEnd",
            Self::TakeScreenshot(_) => "takeScreenshot",
            Self::Back => "back",
            Self::AssertVisible(_) => "assertVisible",
            Self::AssertNotVisible(_) => "assertNotVisible",
            Self::ExtendedWaitUntil(_) => "extendedWaitUntil",
            Self::RunFlow(_) => "runFlow",
            Self::Repeat(_) => "repeat",
            Self::Retry(_) => "retry",
        }
    }

    /// Concise human-readable description used in failure diagnostics and reports.
    pub fn display_label(&self) -> String {
        match self {
            Self::LaunchApp(s) => match &s.app_id {
                Some(id) => format!("Launch app \"{id}\""),
                None => "Launch app".to_string(),
            },
            Self::StopApp(s) => match &s.app_id {
                Some(id) => format!("Stop app \"{id}\""),
                None => "Stop app".to_string(),
            },
            Self::KillApp(s) => match &s.app_id {
                Some(id) => format!("Kill app \"{id}\""),
                None => "Kill app".to_string(),
            },
            Self::ClearState(s) => match &s.app_id {
                Some(id) => format!("Clear state \"{id}\""),
                None => "Clear state".to_string(),
            },
            Self::ClearKeychain => "Clear keychain".to_string(),
            Self::OpenLink(s) => format!("Open link \"{}\"", s.link),
            Self::AddMedia(s) => format!("Add media ({} file(s))", s.files.len()),
            Self::TapOn(s) => s
                .label
                .clone()
                .unwrap_or_else(|| format!("Tap on {}", s.selector.describe())),
            Self::LongPressOn(s) => s
                .label
                .clone()
                .unwrap_or_else(|| format!("Long press on {}", s.selector.describe())),
            Self::DoubleTapOn(s) => s
                .label
                .clone()
                .unwrap_or_else(|| format!("Double tap on {}", s.selector.describe())),
            Self::InputText(_) => "Input text".to_string(),
            Self::EraseText(s) => format!("Erase {} character(s)", s.characters_to_erase),
            Self::PressKey(s) => format!("Press key \"{}\"", s.key),
            Self::HideKeyboard => "Hide keyboard".to_string(),
            Self::Scroll => "Scroll".to_string(),
            Self::ScrollUntilVisible(s) => s
                .label
                .clone()
                .unwrap_or_else(|| format!("Scroll until visible {}", s.element.describe())),
            Self::Swipe(s) => s.label.clone().unwrap_or_else(|| "Swipe".to_string()),
            Self::WaitForAnimationToEnd(_) => "Wait for animation to end".to_string(),
            Self::TakeScreenshot(s) => format!("Take screenshot \"{}\"", s.path),
            Self::Back => "Back".to_string(),
            Self::AssertVisible(s) => s
                .label
                .clone()
                .unwrap_or_else(|| format!("Assert that {} is visible", s.selector.describe())),
            Self::AssertNotVisible(s) => s
                .label
                .clone()
                .unwrap_or_else(|| format!("Assert that {} is not visible", s.selector.describe())),
            Self::ExtendedWaitUntil(s) => s.label.clone().unwrap_or_else(|| {
                if let Some(ref v) = s.visible {
                    format!("Extended wait until {} is visible", v.describe())
                } else if let Some(ref nv) = s.not_visible {
                    format!("Extended wait until {} is not visible", nv.describe())
                } else {
                    "Extended wait until".to_string()
                }
            }),
            Self::RunFlow(s) => s.label.clone().unwrap_or_else(|| match &s.target {
                RunFlowTarget::File(path) => format!("Run flow \"{path}\""),
                RunFlowTarget::Commands(_) => "Run inline flow".to_string(),
            }),
            Self::Repeat(s) => s.label.clone().unwrap_or_else(|| "Repeat".to_string()),
            Self::Retry(s) => s.label.clone().unwrap_or_else(|| match &s.target {
                RunFlowTarget::File(path) => format!("Retry flow \"{path}\""),
                RunFlowTarget::Commands(_) => "Retry inline commands".to_string(),
            }),
        }
    }
}

/// Parses a `maestro/v1` YAML flow document stream (1-doc commands or 2-doc header + commands).
pub fn parse_flow(yaml: &str) -> Result<ParsedFlow, FlowParseError> {
    let docs = parse_yaml_documents(yaml)?;
    match docs.as_slice() {
        [] => Ok(ParsedFlow::default()),
        [doc] => match doc {
            YamlValue::Null => Ok(ParsedFlow::default()),
            YamlValue::Seq(items) => Ok(ParsedFlow {
                header: FlowHeader::default(),
                steps: parse_steps(items, 0)?,
            }),
            _ => Err(FlowParseError::InvalidStructure(
                "single-document flow must be a sequence of commands".to_string(),
            )),
        },
        [header_doc, commands_doc] => {
            let header = match header_doc {
                YamlValue::Null => FlowHeader::default(),
                YamlValue::Map(entries) => parse_header(entries)?,
                _ => {
                    return Err(FlowParseError::InvalidStructure(
                        "flow header document must be a YAML mapping".to_string(),
                    ));
                }
            };
            let steps = match commands_doc {
                YamlValue::Null => Vec::new(),
                YamlValue::Seq(items) => parse_steps(items, 0)?,
                _ => {
                    return Err(FlowParseError::InvalidStructure(
                        "flow commands document must be a YAML sequence or null".to_string(),
                    ));
                }
            };
            Ok(ParsedFlow { header, steps })
        }
        many => Err(FlowParseError::TooManyDocuments(many.len())),
    }
}

fn parse_header(entries: &[(String, YamlValue)]) -> Result<FlowHeader, FlowParseError> {
    let mut header = FlowHeader::default();
    for (key, value) in entries {
        match key.as_str() {
            "appId" => {
                header.app_id = optional_non_empty_string(value, "header.appId")?;
            }
            "url" => {
                header.url = optional_non_empty_string(value, "header.url")?;
            }
            "name" => {
                header.name = optional_non_empty_string(value, "header.name")?;
            }
            "tags" => {
                header.tags = parse_string_list(value, "header.tags")?;
            }
            "env" => {
                header.env = parse_env_map(value, "header.env")?;
            }
            "onFlowStart" => {
                header.on_flow_start = parse_hook_steps(value, "onFlowStart")?;
            }
            "onFlowComplete" => {
                header.on_flow_complete = parse_hook_steps(value, "onFlowComplete")?;
            }
            _ => {
                // Ignore benign metadata fields while strictly checking commands/hooks.
            }
        }
    }
    Ok(header)
}

fn parse_hook_steps(value: &YamlValue, hook_name: &str) -> Result<Vec<FlowStep>, FlowParseError> {
    match value {
        YamlValue::Null => Ok(Vec::new()),
        YamlValue::Seq(items) => parse_steps(items, 1),
        _ => Err(FlowParseError::InvalidStructure(format!(
            "`{hook_name}` must be a sequence of commands"
        ))),
    }
}

fn parse_steps(items: &[YamlValue], depth: usize) -> Result<Vec<FlowStep>, FlowParseError> {
    if depth > MAX_PARSE_DEPTH {
        return Err(FlowParseError::InvalidStructure(format!(
            "command nesting exceeds maximum depth of {MAX_PARSE_DEPTH}"
        )));
    }
    let mut steps = Vec::with_capacity(items.len());
    for item in items {
        steps.push(parse_step(item, depth)?);
    }
    Ok(steps)
}

fn check_explicit_unsupported(command: &str) -> Result<(), FlowParseError> {
    for &(name, reason) in EXPLICIT_UNSUPPORTED_COMMANDS {
        if command == name {
            return Err(FlowParseError::UnsupportedCommand {
                command: command.to_string(),
                reason: reason.to_string(),
            });
        }
    }
    Ok(())
}

fn parse_step(value: &YamlValue, depth: usize) -> Result<FlowStep, FlowParseError> {
    match value {
        YamlValue::String(cmd) => {
            check_explicit_unsupported(cmd)?;
            match cmd.as_str() {
                "launchApp" => Ok(FlowStep::LaunchApp(LaunchAppStep::default())),
                "stopApp" => Ok(FlowStep::StopApp(AppTargetStep::default())),
                "killApp" => Ok(FlowStep::KillApp(AppTargetStep::default())),
                "clearState" => Ok(FlowStep::ClearState(AppTargetStep::default())),
                "clearKeychain" => Ok(FlowStep::ClearKeychain),
                "eraseText" => Ok(FlowStep::EraseText(EraseTextStep {
                    characters_to_erase: DEFAULT_ERASE_TEXT_CHARS,
                })),
                "hideKeyboard" => Ok(FlowStep::HideKeyboard),
                "scroll" => Ok(FlowStep::Scroll),
                "waitForAnimationToEnd" => Ok(FlowStep::WaitForAnimationToEnd(
                    WaitForAnimationToEndStep::default(),
                )),
                "back" => Ok(FlowStep::Back),
                other => Err(FlowParseError::UnsupportedCommand {
                    command: other.to_string(),
                    reason: format!("unknown or bare command `{other}` is not supported"),
                }),
            }
        }
        YamlValue::Map(entries) => {
            if entries.len() != 1 {
                return Err(FlowParseError::InvalidStructure(format!(
                    "each flow step mapping must contain exactly 1 command key, found {}",
                    entries.len()
                )));
            }
            let (command, payload) = &entries[0];
            check_explicit_unsupported(command)?;
            match command.as_str() {
                "launchApp" => Ok(FlowStep::LaunchApp(parse_launch_app(payload)?)),
                "stopApp" => Ok(FlowStep::StopApp(parse_app_target(payload, "stopApp")?)),
                "killApp" => Ok(FlowStep::KillApp(parse_app_target(payload, "killApp")?)),
                "clearState" => Ok(FlowStep::ClearState(parse_app_target(
                    payload,
                    "clearState",
                )?)),
                "clearKeychain" => {
                    ensure_null_or_empty_map(payload, "clearKeychain")?;
                    Ok(FlowStep::ClearKeychain)
                }
                "openLink" => Ok(FlowStep::OpenLink(parse_open_link(payload)?)),
                "addMedia" => Ok(FlowStep::AddMedia(parse_add_media(payload)?)),
                "tapOn" => Ok(FlowStep::TapOn(parse_tap_step(payload, "tapOn", false)?)),
                "longPressOn" => Ok(FlowStep::LongPressOn(parse_tap_step(
                    payload,
                    "longPressOn",
                    true,
                )?)),
                "doubleTapOn" => Ok(FlowStep::DoubleTapOn(parse_tap_step(
                    payload,
                    "doubleTapOn",
                    false,
                )?)),
                "inputText" => Ok(FlowStep::InputText(parse_input_text(payload)?)),
                "eraseText" => Ok(FlowStep::EraseText(parse_erase_text(payload)?)),
                "pressKey" => Ok(FlowStep::PressKey(parse_press_key(payload)?)),
                "hideKeyboard" => {
                    ensure_null_or_empty_map(payload, "hideKeyboard")?;
                    Ok(FlowStep::HideKeyboard)
                }
                "scroll" => {
                    ensure_null_or_empty_map(payload, "scroll")?;
                    Ok(FlowStep::Scroll)
                }
                "scrollUntilVisible" => Ok(FlowStep::ScrollUntilVisible(
                    parse_scroll_until_visible(payload)?,
                )),
                "swipe" => Ok(FlowStep::Swipe(parse_swipe(payload)?)),
                "waitForAnimationToEnd" => Ok(FlowStep::WaitForAnimationToEnd(
                    parse_wait_animation(payload)?,
                )),
                "takeScreenshot" => Ok(FlowStep::TakeScreenshot(parse_take_screenshot(payload)?)),
                "back" => {
                    ensure_null_or_empty_map(payload, "back")?;
                    Ok(FlowStep::Back)
                }
                "assertVisible" => Ok(FlowStep::AssertVisible(parse_assert_visible(payload)?)),
                "assertNotVisible" => Ok(FlowStep::AssertNotVisible(parse_assert_not_visible(
                    payload,
                )?)),
                "extendedWaitUntil" => {
                    Ok(FlowStep::ExtendedWaitUntil(parse_extended_wait(payload)?))
                }
                "runFlow" => Ok(FlowStep::RunFlow(parse_run_flow(payload, depth + 1)?)),
                "repeat" => Ok(FlowStep::Repeat(parse_repeat(payload, depth + 1)?)),
                "retry" => Ok(FlowStep::Retry(parse_retry(payload, depth + 1)?)),
                other => Err(FlowParseError::UnsupportedCommand {
                    command: other.to_string(),
                    reason: format!("unknown Maestro command `{other}`"),
                }),
            }
        }
        _ => Err(FlowParseError::InvalidStructure(
            "each flow step must be a command string or single-key mapping".to_string(),
        )),
    }
}

fn ensure_null_or_empty_map(value: &YamlValue, command: &str) -> Result<(), FlowParseError> {
    match value {
        YamlValue::Null => Ok(()),
        YamlValue::Map(entries) if entries.is_empty() => Ok(()),
        YamlValue::Map(entries) => {
            // Allow benign `label` key on zero-arg commands.
            if entries.iter().all(|(k, _)| k == "label") {
                Ok(())
            } else {
                Err(FlowParseError::InvalidStep {
                    command: command.to_string(),
                    detail: "unexpected parameters on parameterless command".to_string(),
                })
            }
        }
        _ => Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: "expected null or empty mapping".to_string(),
        }),
    }
}

fn yaml_scalar_to_string(value: &YamlValue, context: &str) -> Result<String, FlowParseError> {
    match value {
        YamlValue::String(s) => Ok(s.clone()),
        YamlValue::Bool(b) => Ok(b.to_string()),
        YamlValue::Number(n) => {
            if !n.is_finite() {
                return Err(FlowParseError::InvalidStep {
                    command: context.to_string(),
                    detail: "non-finite number cannot be converted to scalar text".to_string(),
                });
            }
            Ok(js_number_to_string(*n))
        }
        _ => Err(FlowParseError::InvalidStep {
            command: context.to_string(),
            detail: "expected string or scalar value".to_string(),
        }),
    }
}

fn optional_non_empty_string(
    value: &YamlValue,
    context: &str,
) -> Result<Option<String>, FlowParseError> {
    match value {
        YamlValue::Null => Ok(None),
        _ => {
            let s = yaml_scalar_to_string(value, context)?;
            if s.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(s))
            }
        }
    }
}

fn yaml_to_bool(value: &YamlValue, command: &str, field: &str) -> Result<bool, FlowParseError> {
    match value {
        YamlValue::Bool(b) => Ok(*b),
        _ => Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: format!("field `{field}` must be a boolean"),
        }),
    }
}

fn yaml_to_u64(value: &YamlValue, command: &str, field: &str) -> Result<u64, FlowParseError> {
    match value {
        YamlValue::Number(n) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 => {
            if *n > (u64::MAX as f64) {
                return Err(FlowParseError::InvalidStep {
                    command: command.to_string(),
                    detail: format!("field `{field}` exceeds integer range"),
                });
            }
            Ok(*n as u64)
        }
        YamlValue::String(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| FlowParseError::InvalidStep {
                command: command.to_string(),
                detail: format!("field `{field}` must be a non-negative integer"),
            }),
        _ => Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: format!("field `{field}` must be a non-negative integer"),
        }),
    }
}

fn yaml_to_u32(value: &YamlValue, command: &str, field: &str) -> Result<u32, FlowParseError> {
    let v = yaml_to_u64(value, command, field)?;
    u32::try_from(v).map_err(|_| FlowParseError::InvalidStep {
        command: command.to_string(),
        detail: format!("field `{field}` exceeds 32-bit integer range"),
    })
}

fn yaml_to_usize(value: &YamlValue, command: &str, field: &str) -> Result<usize, FlowParseError> {
    let v = yaml_to_u64(value, command, field)?;
    usize::try_from(v).map_err(|_| FlowParseError::InvalidStep {
        command: command.to_string(),
        detail: format!("field `{field}` exceeds usize range"),
    })
}

fn yaml_to_launch_arg(
    value: &YamlValue,
    command: &str,
) -> Result<LaunchArgumentValue, FlowParseError> {
    match value {
        YamlValue::String(s) => Ok(LaunchArgumentValue::String(s.clone())),
        YamlValue::Bool(b) => Ok(LaunchArgumentValue::Bool(*b)),
        YamlValue::Number(n) if n.is_finite() => {
            let num = if n.fract() == 0.0 && *n >= (i64::MIN as f64) && *n <= (i64::MAX as f64) {
                serde_json::Number::from(*n as i64)
            } else {
                serde_json::Number::from_f64(*n).ok_or_else(|| FlowParseError::InvalidStep {
                    command: command.to_string(),
                    detail: "launch argument number must be finite".to_string(),
                })?
            };
            Ok(LaunchArgumentValue::Number(num))
        }
        _ => Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: "launch argument values must be strings, finite numbers, or booleans"
                .to_string(),
        }),
    }
}

fn parse_string_list(value: &YamlValue, context: &str) -> Result<Vec<String>, FlowParseError> {
    let Some(items) = value.as_seq() else {
        return Err(FlowParseError::InvalidStructure(format!(
            "`{context}` must be a sequence"
        )));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(yaml_scalar_to_string(item, context)?);
    }
    Ok(out)
}

fn parse_env_map(
    value: &YamlValue,
    context: &str,
) -> Result<BTreeMap<String, String>, FlowParseError> {
    let Some(entries) = value.as_map() else {
        return Err(FlowParseError::InvalidStructure(format!(
            "`{context}` must be a YAML mapping"
        )));
    };
    let mut out = BTreeMap::new();
    for (k, v) in entries {
        if matches!(v, YamlValue::Null) {
            out.insert(k.clone(), String::new());
        } else {
            out.insert(k.clone(), yaml_scalar_to_string(v, context)?);
        }
    }
    Ok(out)
}

fn parse_launch_app(value: &YamlValue) -> Result<LaunchAppStep, FlowParseError> {
    match value {
        YamlValue::Null => Ok(LaunchAppStep::default()),
        YamlValue::String(app_id) => Ok(LaunchAppStep {
            app_id: Some(app_id.clone()),
            ..LaunchAppStep::default()
        }),
        YamlValue::Map(entries) => {
            let mut step = LaunchAppStep::default();
            for (k, v) in entries {
                match k.as_str() {
                    "appId" => step.app_id = optional_non_empty_string(v, "launchApp.appId")?,
                    "clearState" => step.clear_state = yaml_to_bool(v, "launchApp", "clearState")?,
                    "clearKeychain" => {
                        step.clear_keychain = yaml_to_bool(v, "launchApp", "clearKeychain")?;
                    }
                    "stopApp" => step.stop_app = yaml_to_bool(v, "launchApp", "stopApp")?,
                    "permissions" => {
                        step.permissions = Some(parse_env_map(v, "launchApp.permissions")?);
                    }
                    "arguments" => {
                        let Some(arg_entries) = v.as_map() else {
                            return Err(FlowParseError::InvalidStep {
                                command: "launchApp".to_string(),
                                detail: "`arguments` must be a YAML mapping".to_string(),
                            });
                        };
                        for (ak, av) in arg_entries {
                            step.arguments
                                .insert(ak.clone(), yaml_to_launch_arg(av, "launchApp")?);
                        }
                    }
                    "label" => {}
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: "launchApp".to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }
            Ok(step)
        }
        _ => Err(FlowParseError::InvalidStep {
            command: "launchApp".to_string(),
            detail: "expected null, appId string, or mapping".to_string(),
        }),
    }
}

fn parse_app_target(value: &YamlValue, command: &str) -> Result<AppTargetStep, FlowParseError> {
    match value {
        YamlValue::Null => Ok(AppTargetStep::default()),
        YamlValue::String(app_id) => Ok(AppTargetStep {
            app_id: Some(app_id.clone()),
        }),
        YamlValue::Map(entries) => {
            let mut step = AppTargetStep::default();
            for (k, v) in entries {
                match k.as_str() {
                    "appId" => step.app_id = optional_non_empty_string(v, command)?,
                    "label" => {}
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: command.to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }
            Ok(step)
        }
        _ => Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: "expected null, appId string, or mapping".to_string(),
        }),
    }
}

fn parse_open_link(value: &YamlValue) -> Result<OpenLinkStep, FlowParseError> {
    match value {
        YamlValue::String(link) => {
            if link.trim().is_empty() {
                return Err(FlowParseError::InvalidStep {
                    command: "openLink".to_string(),
                    detail: "link URL must not be empty".to_string(),
                });
            }
            Ok(OpenLinkStep {
                link: link.clone(),
                auto_verify: None,
                browser: None,
            })
        }
        YamlValue::Map(entries) => {
            let mut link: Option<String> = None;
            let mut auto_verify: Option<bool> = None;
            let mut browser: Option<bool> = None;
            for (k, v) in entries {
                match k.as_str() {
                    "link" => link = optional_non_empty_string(v, "openLink.link")?,
                    "autoVerify" => {
                        auto_verify = Some(yaml_to_bool(v, "openLink", "autoVerify")?);
                    }
                    "browser" => browser = Some(yaml_to_bool(v, "openLink", "browser")?),
                    "label" => {}
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: "openLink".to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }
            let Some(link) = link else {
                return Err(FlowParseError::InvalidStep {
                    command: "openLink".to_string(),
                    detail: "missing required `link` URL".to_string(),
                });
            };
            Ok(OpenLinkStep {
                link,
                auto_verify,
                browser,
            })
        }
        _ => Err(FlowParseError::InvalidStep {
            command: "openLink".to_string(),
            detail: "expected URL string or mapping".to_string(),
        }),
    }
}

fn parse_add_media(value: &YamlValue) -> Result<AddMediaStep, FlowParseError> {
    let seq = match value {
        YamlValue::Seq(items) => items.as_slice(),
        YamlValue::Map(_) => value
            .get("files")
            .and_then(YamlValue::as_seq)
            .ok_or_else(|| FlowParseError::InvalidStep {
                command: "addMedia".to_string(),
                detail: "expected a `files` sequence".to_string(),
            })?,
        _ => {
            return Err(FlowParseError::InvalidStep {
                command: "addMedia".to_string(),
                detail: "expected a sequence of file paths or mapping with `files`".to_string(),
            });
        }
    };
    if seq.is_empty() {
        return Err(FlowParseError::InvalidStep {
            command: "addMedia".to_string(),
            detail: "media file list must not be empty".to_string(),
        });
    }
    let mut files = Vec::with_capacity(seq.len());
    for item in seq {
        let raw = yaml_scalar_to_string(item, "addMedia")?;
        let trimmed = trim_es_whitespace(&raw);
        if trimmed.is_empty() {
            return Err(FlowParseError::InvalidStep {
                command: "addMedia".to_string(),
                detail: "media path must not be empty".to_string(),
            });
        }
        files.push(trimmed.to_string());
    }
    Ok(AddMediaStep { files })
}

struct ParsedSelector {
    selector: Selector,
    timeout_ms: Option<u64>,
    duration_ms: Option<u64>,
    label: Option<String>,
}

fn parse_selector(
    value: &YamlValue,
    command: &str,
    allow_point: bool,
    depth: usize,
) -> Result<ParsedSelector, FlowParseError> {
    if depth > MAX_PARSE_DEPTH {
        return Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: "selector nesting exceeds maximum depth".to_string(),
        });
    }
    match value {
        YamlValue::String(_) | YamlValue::Number(_) | YamlValue::Bool(_) => {
            let text = yaml_scalar_to_string(value, command)?;
            Ok(ParsedSelector {
                selector: Selector::by_text(text),
                timeout_ms: None,
                duration_ms: None,
                label: None,
            })
        }
        YamlValue::Map(entries) => {
            let mut sel = Selector::default();
            let mut timeout_ms: Option<u64> = None;
            let mut duration_ms: Option<u64> = None;
            let mut label: Option<String> = None;

            for (k, v) in entries {
                match k.as_str() {
                    "id" => sel.id = Some(yaml_scalar_to_string(v, command)?),
                    "text" => sel.text = Some(yaml_scalar_to_string(v, command)?),
                    "index" => sel.index = Some(yaml_to_usize(v, command, "index")?),
                    "enabled" => sel.enabled = Some(yaml_to_bool(v, command, "enabled")?),
                    "checked" => sel.checked = Some(yaml_to_bool(v, command, "checked")?),
                    "focused" => sel.focused = Some(yaml_to_bool(v, command, "focused")?),
                    "selected" => sel.selected = Some(yaml_to_bool(v, command, "selected")?),
                    "point" => {
                        if !allow_point {
                            return Err(FlowParseError::InvalidStep {
                                command: command.to_string(),
                                detail: "`point` is not supported on this command".to_string(),
                            });
                        }
                        let raw = yaml_scalar_to_string(v, command)?;
                        sel.point = Some(PointSpec::parse(&raw)?);
                    }
                    "below" => {
                        let nested = parse_selector(v, command, false, depth + 1)?.selector;
                        sel.below = Some(Box::new(nested));
                    }
                    "above" => {
                        let nested = parse_selector(v, command, false, depth + 1)?.selector;
                        sel.above = Some(Box::new(nested));
                    }
                    "leftOf" => {
                        let nested = parse_selector(v, command, false, depth + 1)?.selector;
                        sel.left_of = Some(Box::new(nested));
                    }
                    "rightOf" => {
                        let nested = parse_selector(v, command, false, depth + 1)?.selector;
                        sel.right_of = Some(Box::new(nested));
                    }
                    "childOf" => {
                        let nested = parse_selector(v, command, false, depth + 1)?.selector;
                        sel.child_of = Some(Box::new(nested));
                    }
                    "containsChild" => {
                        let nested = parse_selector(v, command, false, depth + 1)?.selector;
                        sel.contains_child = Some(Box::new(nested));
                    }
                    "containsDescendants" => {
                        let Some(items) = v.as_seq() else {
                            return Err(FlowParseError::InvalidStep {
                                command: command.to_string(),
                                detail: "`containsDescendants` must be a sequence of selectors"
                                    .to_string(),
                            });
                        };
                        for item in items {
                            let nested = parse_selector(item, command, false, depth + 1)?.selector;
                            sel.contains_descendants.push(nested);
                        }
                    }
                    "optional" => sel.optional = yaml_to_bool(v, command, "optional")?,
                    "waitToSettleTimeoutMs" => {
                        sel.wait_to_settle_timeout_ms =
                            Some(yaml_to_u64(v, command, "waitToSettleTimeoutMs")?);
                    }
                    "retryTapIfNoChange" => {
                        sel.retry_tap_if_no_change =
                            Some(yaml_to_bool(v, command, "retryTapIfNoChange")?);
                    }
                    "repeat" => sel.repeat = Some(yaml_to_u32(v, command, "repeat")?),
                    "delay" => sel.delay_ms = Some(yaml_to_u64(v, command, "delay")?),
                    "timeout" => timeout_ms = Some(yaml_to_u64(v, command, "timeout")?),
                    "duration" => duration_ms = Some(yaml_to_u64(v, command, "duration")?),
                    "label" => label = optional_non_empty_string(v, command)?,
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: command.to_string(),
                            detail: format!("unknown selector field `{other}`"),
                        });
                    }
                }
            }
            if !sel.has_element_criteria() && sel.point.is_none() {
                return Err(FlowParseError::Selector(SelectorError::EmptySelector));
            }
            Ok(ParsedSelector {
                selector: sel,
                timeout_ms,
                duration_ms,
                label,
            })
        }
        _ => Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: "expected selector string or mapping".to_string(),
        }),
    }
}

fn parse_tap_step(
    value: &YamlValue,
    command: &str,
    allow_duration: bool,
) -> Result<TapStep, FlowParseError> {
    let ParsedSelector {
        selector,
        duration_ms,
        label,
        ..
    } = parse_selector(value, command, true, 0)?;
    if !allow_duration && duration_ms.is_some() {
        return Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: "`duration` is only supported on `longPressOn`".to_string(),
        });
    }
    Ok(TapStep {
        selector,
        duration_ms,
        label,
    })
}

fn parse_input_text(value: &YamlValue) -> Result<InputTextStep, FlowParseError> {
    match value {
        YamlValue::Map(entries) => {
            let mut text: Option<String> = None;
            for (k, v) in entries {
                match k.as_str() {
                    "text" => text = Some(yaml_scalar_to_string(v, "inputText")?),
                    "label" => {}
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: "inputText".to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }
            let Some(text) = text else {
                return Err(FlowParseError::InvalidStep {
                    command: "inputText".to_string(),
                    detail: "missing required `text` field".to_string(),
                });
            };
            Ok(InputTextStep { text })
        }
        other => Ok(InputTextStep {
            text: yaml_scalar_to_string(other, "inputText")?,
        }),
    }
}

fn parse_erase_text(value: &YamlValue) -> Result<EraseTextStep, FlowParseError> {
    match value {
        YamlValue::Null => Ok(EraseTextStep {
            characters_to_erase: DEFAULT_ERASE_TEXT_CHARS,
        }),
        YamlValue::Number(_) | YamlValue::String(_) => Ok(EraseTextStep {
            characters_to_erase: yaml_to_u32(value, "eraseText", "charactersToErase")?,
        }),
        YamlValue::Map(entries) => {
            let mut count = DEFAULT_ERASE_TEXT_CHARS;
            for (k, v) in entries {
                match k.as_str() {
                    "charactersToErase" => {
                        count = yaml_to_u32(v, "eraseText", "charactersToErase")?;
                    }
                    "label" => {}
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: "eraseText".to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }
            Ok(EraseTextStep {
                characters_to_erase: count,
            })
        }
        _ => Err(FlowParseError::InvalidStep {
            command: "eraseText".to_string(),
            detail: "expected integer, null, or mapping".to_string(),
        }),
    }
}

fn parse_press_key(value: &YamlValue) -> Result<PressKeyStep, FlowParseError> {
    match value {
        YamlValue::String(key) => {
            if key.trim().is_empty() {
                return Err(FlowParseError::InvalidStep {
                    command: "pressKey".to_string(),
                    detail: "key name must not be empty".to_string(),
                });
            }
            Ok(PressKeyStep {
                key: key.clone(),
                modifiers: Vec::new(),
                count: 1,
            })
        }
        YamlValue::Map(entries) => {
            let mut key: Option<String> = None;
            let mut modifiers: Vec<String> = Vec::new();
            let mut count: u32 = 1;
            for (k, v) in entries {
                match k.as_str() {
                    "key" => key = optional_non_empty_string(v, "pressKey.key")?,
                    "modifiers" => modifiers = parse_string_list(v, "pressKey.modifiers")?,
                    "count" => count = yaml_to_u32(v, "pressKey", "count")?.max(1),
                    "label" => {}
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: "pressKey".to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }
            let Some(key) = key else {
                return Err(FlowParseError::InvalidStep {
                    command: "pressKey".to_string(),
                    detail: "missing required `key` field".to_string(),
                });
            };
            Ok(PressKeyStep {
                key,
                modifiers,
                count,
            })
        }
        _ => Err(FlowParseError::InvalidStep {
            command: "pressKey".to_string(),
            detail: "expected key string or mapping".to_string(),
        }),
    }
}

fn parse_scroll_until_visible(value: &YamlValue) -> Result<ScrollUntilVisibleStep, FlowParseError> {
    let Some(entries) = value.as_map() else {
        return Err(FlowParseError::InvalidStep {
            command: "scrollUntilVisible".to_string(),
            detail: "expected a YAML mapping".to_string(),
        });
    };
    let mut element: Option<Selector> = None;
    let mut direction = ScrollDirection::Down;
    let mut timeout_ms = DEFAULT_SCROLL_TIMEOUT_MS;
    let mut speed = DEFAULT_SCROLL_SPEED;
    let mut visibility_percentage = DEFAULT_VISIBILITY_PERCENTAGE;
    let mut center_element = false;
    let mut label = None;

    for (k, v) in entries {
        match k.as_str() {
            "element" => {
                element = Some(parse_selector(v, "scrollUntilVisible", false, 0)?.selector);
            }
            "direction" => {
                let raw = yaml_scalar_to_string(v, "scrollUntilVisible")?;
                direction = ScrollDirection::parse(&raw, "scrollUntilVisible")?;
            }
            "timeout" => timeout_ms = yaml_to_u64(v, "scrollUntilVisible", "timeout")?,
            "speed" => {
                let raw = yaml_to_u64(v, "scrollUntilVisible", "speed")?;
                speed = raw.clamp(1, 100) as u8;
            }
            "visibilityPercentage" => {
                let raw = yaml_to_u64(v, "scrollUntilVisible", "visibilityPercentage")?;
                visibility_percentage = raw.clamp(1, 100) as u8;
            }
            "centerElement" => {
                center_element = yaml_to_bool(v, "scrollUntilVisible", "centerElement")?;
            }
            "label" => label = optional_non_empty_string(v, "scrollUntilVisible")?,
            other => {
                return Err(FlowParseError::InvalidStep {
                    command: "scrollUntilVisible".to_string(),
                    detail: format!("unknown field `{other}`"),
                });
            }
        }
    }

    let Some(element) = element else {
        return Err(FlowParseError::InvalidStep {
            command: "scrollUntilVisible".to_string(),
            detail: "missing required `element` selector".to_string(),
        });
    };

    Ok(ScrollUntilVisibleStep {
        element,
        direction,
        timeout_ms,
        speed,
        visibility_percentage,
        center_element,
        label,
    })
}

fn parse_swipe(value: &YamlValue) -> Result<SwipeStep, FlowParseError> {
    let Some(entries) = value.as_map() else {
        return Err(FlowParseError::InvalidStep {
            command: "swipe".to_string(),
            detail: "expected a YAML mapping".to_string(),
        });
    };
    let mut direction = None;
    let mut start = None;
    let mut end = None;
    let mut from = None;
    let mut duration_ms = DEFAULT_SWIPE_DURATION_MS;
    let mut label = None;

    for (k, v) in entries {
        match k.as_str() {
            "direction" => {
                let raw = yaml_scalar_to_string(v, "swipe")?;
                direction = Some(ScrollDirection::parse(&raw, "swipe")?);
            }
            "start" => {
                let raw = yaml_scalar_to_string(v, "swipe")?;
                start = Some(PointSpec::parse(&raw)?);
            }
            "end" => {
                let raw = yaml_scalar_to_string(v, "swipe")?;
                end = Some(PointSpec::parse(&raw)?);
            }
            "from" => {
                from = Some(parse_selector(v, "swipe", false, 0)?.selector);
            }
            "duration" => duration_ms = yaml_to_u64(v, "swipe", "duration")?,
            "label" => label = optional_non_empty_string(v, "swipe")?,
            other => {
                return Err(FlowParseError::InvalidStep {
                    command: "swipe".to_string(),
                    detail: format!("unknown field `{other}`"),
                });
            }
        }
    }

    let has_dir = direction.is_some();
    let has_coords = start.is_some() && end.is_some();
    if !has_dir && !has_coords {
        return Err(FlowParseError::InvalidStep {
            command: "swipe".to_string(),
            detail: "requires either `direction` or both `start` and `end`".to_string(),
        });
    }
    if start.is_some() != end.is_some() {
        return Err(FlowParseError::InvalidStep {
            command: "swipe".to_string(),
            detail: "`start` and `end` must both be supplied together".to_string(),
        });
    }

    Ok(SwipeStep {
        direction,
        start,
        end,
        from,
        duration_ms,
        label,
    })
}

fn parse_wait_animation(value: &YamlValue) -> Result<WaitForAnimationToEndStep, FlowParseError> {
    match value {
        YamlValue::Null => Ok(WaitForAnimationToEndStep::default()),
        YamlValue::Map(entries) => {
            let mut timeout_ms = None;
            for (k, v) in entries {
                match k.as_str() {
                    "timeout" => {
                        timeout_ms = Some(yaml_to_u64(v, "waitForAnimationToEnd", "timeout")?);
                    }
                    "label" => {}
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: "waitForAnimationToEnd".to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }
            Ok(WaitForAnimationToEndStep { timeout_ms })
        }
        _ => Err(FlowParseError::InvalidStep {
            command: "waitForAnimationToEnd".to_string(),
            detail: "expected null or mapping".to_string(),
        }),
    }
}

fn parse_take_screenshot(value: &YamlValue) -> Result<TakeScreenshotStep, FlowParseError> {
    let path = match value {
        YamlValue::String(s) => s.clone(),
        YamlValue::Map(entries) => {
            let mut found = None;
            for (k, v) in entries {
                match k.as_str() {
                    "path" => found = optional_non_empty_string(v, "takeScreenshot.path")?,
                    "label" => {}
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: "takeScreenshot".to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }
            found.ok_or_else(|| FlowParseError::InvalidStep {
                command: "takeScreenshot".to_string(),
                detail: "missing required `path` field".to_string(),
            })?
        }
        _ => {
            return Err(FlowParseError::InvalidStep {
                command: "takeScreenshot".to_string(),
                detail: "expected path string or mapping".to_string(),
            });
        }
    };
    if path.trim().is_empty() {
        return Err(FlowParseError::InvalidStep {
            command: "takeScreenshot".to_string(),
            detail: "screenshot path must not be empty".to_string(),
        });
    }
    Ok(TakeScreenshotStep { path })
}

fn parse_assert_visible(value: &YamlValue) -> Result<AssertVisibleStep, FlowParseError> {
    let ParsedSelector {
        selector,
        timeout_ms,
        label,
        ..
    } = parse_selector(value, "assertVisible", false, 0)?;
    Ok(AssertVisibleStep {
        selector,
        timeout_ms,
        label,
    })
}

fn parse_assert_not_visible(value: &YamlValue) -> Result<AssertNotVisibleStep, FlowParseError> {
    let ParsedSelector {
        selector,
        timeout_ms,
        label,
        ..
    } = parse_selector(value, "assertNotVisible", false, 0)?;
    Ok(AssertNotVisibleStep {
        selector,
        timeout_ms,
        label,
    })
}

fn parse_extended_wait(value: &YamlValue) -> Result<ExtendedWaitUntilStep, FlowParseError> {
    let Some(entries) = value.as_map() else {
        return Err(FlowParseError::InvalidStep {
            command: "extendedWaitUntil".to_string(),
            detail: "expected a YAML mapping".to_string(),
        });
    };
    let mut visible = None;
    let mut not_visible = None;
    let mut timeout_ms = DEFAULT_EXTENDED_WAIT_TIMEOUT_MS;
    let mut label = None;

    for (k, v) in entries {
        match k.as_str() {
            "visible" => {
                visible = Some(parse_selector(v, "extendedWaitUntil", false, 0)?.selector);
            }
            "notVisible" => {
                not_visible = Some(parse_selector(v, "extendedWaitUntil", false, 0)?.selector);
            }
            "timeout" => timeout_ms = yaml_to_u64(v, "extendedWaitUntil", "timeout")?,
            "label" => label = optional_non_empty_string(v, "extendedWaitUntil")?,
            other => {
                return Err(FlowParseError::InvalidStep {
                    command: "extendedWaitUntil".to_string(),
                    detail: format!("unknown field `{other}`"),
                });
            }
        }
    }

    match (&visible, &not_visible) {
        (Some(_), None) | (None, Some(_)) => Ok(ExtendedWaitUntilStep {
            visible,
            not_visible,
            timeout_ms,
            label,
        }),
        (None, None) => Err(FlowParseError::InvalidStep {
            command: "extendedWaitUntil".to_string(),
            detail: "requires either `visible` or `notVisible`".to_string(),
        }),
        (Some(_), Some(_)) => Err(FlowParseError::InvalidStep {
            command: "extendedWaitUntil".to_string(),
            detail: "cannot specify both `visible` and `notVisible`".to_string(),
        }),
    }
}

fn parse_condition(value: &YamlValue, context: &str) -> Result<Condition, FlowParseError> {
    let Some(entries) = value.as_map() else {
        return Err(FlowParseError::InvalidStep {
            command: context.to_string(),
            detail: "condition must be a YAML mapping".to_string(),
        });
    };
    let mut cond = Condition::default();
    for (k, v) in entries {
        match k.as_str() {
            "true" => {
                return Err(FlowParseError::UnsupportedCommand {
                    command: format!("{context}.true"),
                    reason: "JavaScript expression conditions (`when.true` / `while.true`) are not supported"
                        .to_string(),
                });
            }
            "visible" => {
                cond.visible = Some(parse_selector(v, context, false, 0)?.selector);
            }
            "notVisible" => {
                cond.not_visible = Some(parse_selector(v, context, false, 0)?.selector);
            }
            "platform" => {
                let raw = yaml_scalar_to_string(v, context)?;
                if raw.trim().is_empty() {
                    return Err(FlowParseError::InvalidStep {
                        command: context.to_string(),
                        detail: "`platform` condition must not be empty".to_string(),
                    });
                }
                cond.platform = Some(raw.trim().to_ascii_lowercase());
            }
            other => {
                return Err(FlowParseError::InvalidStep {
                    command: context.to_string(),
                    detail: format!("unknown condition key `{other}`"),
                });
            }
        }
    }
    if cond.visible.is_none() && cond.not_visible.is_none() && cond.platform.is_none() {
        return Err(FlowParseError::InvalidStep {
            command: context.to_string(),
            detail: "condition must specify at least one of `visible`, `notVisible`, or `platform`"
                .to_string(),
        });
    }
    Ok(cond)
}

fn parse_reference_file_path(value: &YamlValue, command: &str) -> Result<String, FlowParseError> {
    let YamlValue::String(raw) = value else {
        return Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: "subflow file path must be a string".to_string(),
        });
    };
    let trimmed = trim_es_whitespace(raw);
    if trimmed.is_empty() {
        return Err(FlowParseError::InvalidStep {
            command: command.to_string(),
            detail: "subflow file path must not be empty".to_string(),
        });
    }
    Ok(trimmed.to_string())
}

fn parse_run_flow(value: &YamlValue, depth: usize) -> Result<RunFlowStep, FlowParseError> {
    match value {
        YamlValue::String(_) => {
            let trimmed = parse_reference_file_path(value, "runFlow")?;
            Ok(RunFlowStep {
                target: RunFlowTarget::File(trimmed),
                env: BTreeMap::new(),
                when: None,
                optional: false,
                label: None,
            })
        }
        YamlValue::Map(entries) => {
            let mut file: Option<String> = None;
            let mut commands: Option<Vec<FlowStep>> = None;
            let mut env = BTreeMap::new();
            let mut when = None;
            let mut optional = false;
            let mut label = None;

            for (k, v) in entries {
                match k.as_str() {
                    "file" => file = Some(parse_reference_file_path(v, "runFlow.file")?),
                    "commands" => {
                        let Some(items) = v.as_seq() else {
                            return Err(FlowParseError::InvalidStep {
                                command: "runFlow".to_string(),
                                detail: "`commands` must be a sequence".to_string(),
                            });
                        };
                        commands = Some(parse_steps(items, depth)?);
                    }
                    "env" => env = parse_env_map(v, "runFlow.env")?,
                    "when" => when = Some(parse_condition(v, "when")?),
                    "optional" => optional = yaml_to_bool(v, "runFlow", "optional")?,
                    "label" => label = optional_non_empty_string(v, "runFlow")?,
                    other => {
                        return Err(FlowParseError::InvalidStep {
                            command: "runFlow".to_string(),
                            detail: format!("unknown field `{other}`"),
                        });
                    }
                }
            }

            let target = match (file, commands) {
                (Some(f), None) => RunFlowTarget::File(f),
                (None, Some(cmds)) => RunFlowTarget::Commands(cmds),
                (Some(_), Some(_)) => {
                    return Err(FlowParseError::InvalidStep {
                        command: "runFlow".to_string(),
                        detail: "cannot specify both `file` and inline `commands`".to_string(),
                    });
                }
                (None, None) => {
                    return Err(FlowParseError::InvalidStep {
                        command: "runFlow".to_string(),
                        detail: "requires either `file` or inline `commands`".to_string(),
                    });
                }
            };

            Ok(RunFlowStep {
                target,
                env,
                when,
                optional,
                label,
            })
        }
        _ => Err(FlowParseError::InvalidStep {
            command: "runFlow".to_string(),
            detail: "expected file path string or mapping".to_string(),
        }),
    }
}

fn parse_repeat(value: &YamlValue, depth: usize) -> Result<RepeatStep, FlowParseError> {
    let Some(entries) = value.as_map() else {
        return Err(FlowParseError::InvalidStep {
            command: "repeat".to_string(),
            detail: "expected a YAML mapping".to_string(),
        });
    };
    let mut times = None;
    let mut while_cond = None;
    let mut commands = None;
    let mut label = None;

    for (k, v) in entries {
        match k.as_str() {
            "times" => times = Some(yaml_to_u32(v, "repeat", "times")?),
            "while" => while_cond = Some(parse_condition(v, "while")?),
            "commands" => {
                let Some(items) = v.as_seq() else {
                    return Err(FlowParseError::InvalidStep {
                        command: "repeat".to_string(),
                        detail: "`commands` must be a sequence".to_string(),
                    });
                };
                commands = Some(parse_steps(items, depth)?);
            }
            "label" => label = optional_non_empty_string(v, "repeat")?,
            other => {
                return Err(FlowParseError::InvalidStep {
                    command: "repeat".to_string(),
                    detail: format!("unknown field `{other}`"),
                });
            }
        }
    }

    if times.is_none() && while_cond.is_none() {
        return Err(FlowParseError::InvalidStep {
            command: "repeat".to_string(),
            detail: "requires `times` and/or `while`".to_string(),
        });
    }
    let Some(commands) = commands else {
        return Err(FlowParseError::InvalidStep {
            command: "repeat".to_string(),
            detail: "missing required `commands` sequence".to_string(),
        });
    };

    Ok(RepeatStep {
        times,
        while_cond,
        commands,
        label,
    })
}

fn parse_retry(value: &YamlValue, depth: usize) -> Result<RetryStep, FlowParseError> {
    let Some(entries) = value.as_map() else {
        return Err(FlowParseError::InvalidStep {
            command: "retry".to_string(),
            detail: "expected a YAML mapping".to_string(),
        });
    };
    let mut max_retries: u32 = 1;
    let mut file: Option<String> = None;
    let mut commands: Option<Vec<FlowStep>> = None;
    let mut env = BTreeMap::new();
    let mut label = None;

    for (k, v) in entries {
        match k.as_str() {
            "maxRetries" => max_retries = yaml_to_u32(v, "retry", "maxRetries")?,
            "file" => file = Some(parse_reference_file_path(v, "retry.file")?),
            "commands" => {
                let Some(items) = v.as_seq() else {
                    return Err(FlowParseError::InvalidStep {
                        command: "retry".to_string(),
                        detail: "`commands` must be a sequence".to_string(),
                    });
                };
                commands = Some(parse_steps(items, depth)?);
            }
            "env" => env = parse_env_map(v, "retry.env")?,
            "label" => label = optional_non_empty_string(v, "retry")?,
            other => {
                return Err(FlowParseError::InvalidStep {
                    command: "retry".to_string(),
                    detail: format!("unknown field `{other}`"),
                });
            }
        }
    }

    let target = match (file, commands) {
        (Some(f), None) => RunFlowTarget::File(f),
        (None, Some(cmds)) => RunFlowTarget::Commands(cmds),
        (Some(_), Some(_)) => {
            return Err(FlowParseError::InvalidStep {
                command: "retry".to_string(),
                detail: "cannot specify both `file` and inline `commands`".to_string(),
            });
        }
        (None, None) => {
            return Err(FlowParseError::InvalidStep {
                command: "retry".to_string(),
                detail: "requires either `file` or inline `commands`".to_string(),
            });
        }
    };

    Ok(RetryStep {
        max_retries,
        target,
        env,
        label,
    })
}

#[cfg(test)]
mod tests {
    use super::{FlowStep, parse_flow};
    use std::collections::BTreeMap;

    #[test]
    fn parse_launch_app_distinguishes_omitted_empty_and_explicit_permissions() {
        let yaml = r#"appId: dev.deja.example
---
- launchApp
- launchApp:
    appId: dev.deja.example
    permissions: {}
- launchApp:
    appId: dev.deja.example
    permissions:
      all: deny
      location: allow
"#;
        let parsed = parse_flow(yaml).expect("parse flow yaml");
        assert_eq!(parsed.steps.len(), 3);

        match &parsed.steps[0] {
            FlowStep::LaunchApp(step) => assert_eq!(step.permissions, None),
            other => panic!("expected LaunchApp, got {other:?}"),
        }
        match &parsed.steps[1] {
            FlowStep::LaunchApp(step) => assert_eq!(step.permissions, Some(BTreeMap::new())),
            other => panic!("expected LaunchApp, got {other:?}"),
        }
        match &parsed.steps[2] {
            FlowStep::LaunchApp(step) => {
                let mut expected = BTreeMap::new();
                expected.insert("all".to_string(), "deny".to_string());
                expected.insert("location".to_string(), "allow".to_string());
                assert_eq!(step.permissions, Some(expected));
            }
            other => panic!("expected LaunchApp, got {other:?}"),
        }
    }
}
