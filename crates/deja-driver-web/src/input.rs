//! Strict loopback URL validation, viewport `ScreenSize` parsing, and keyboard/modifier mapping
//! for Chromium CDP (`Input.dispatchKeyEvent`).

use deja_flow::KeyPress;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, WebDriverError};

/// Default Chromium viewport width in CSS pixels.
pub const DEFAULT_VIEWPORT_WIDTH: u32 = 1280;
/// Default Chromium viewport height in CSS pixels.
pub const DEFAULT_VIEWPORT_HEIGHT: u32 = 720;
/// Minimum allowed viewport dimension in CSS pixels.
pub const MIN_VIEWPORT_DIM: u32 = 100;
/// Maximum allowed viewport width in CSS pixels (8K UHD).
pub const MAX_VIEWPORT_WIDTH: u32 = 7680;
/// Maximum allowed viewport height in CSS pixels (8K UHD).
pub const MAX_VIEWPORT_HEIGHT: u32 = 4320;
/// Upper bound on key repeat count in a single `press_key` or `erase_text` call.
pub const MAX_KEY_REPEAT_COUNT: u32 = 10_000;

/// CDP modifier bitmask constants (`Input.dispatchKeyEvent` `modifiers` field).
pub const MODIFIER_ALT: u8 = 1;
pub const MODIFIER_CTRL: u8 = 2;
pub const MODIFIER_META: u8 = 4;
pub const MODIFIER_SHIFT: u8 = 8;

/// Validated Chromium viewport dimensions in logical CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSize {
    pub width: u32,
    pub height: u32,
}

impl Default for ScreenSize {
    fn default() -> Self {
        Self {
            width: DEFAULT_VIEWPORT_WIDTH,
            height: DEFAULT_VIEWPORT_HEIGHT,
        }
    }
}

impl ScreenSize {
    /// Constructs a [`ScreenSize`], validating `100..=7680` × `100..=4320`.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if !(MIN_VIEWPORT_DIM..=MAX_VIEWPORT_WIDTH).contains(&width) {
            return Err(WebDriverError::InvalidScreenSize(format!(
                "width {width} is out of bounds ({MIN_VIEWPORT_DIM}..={MAX_VIEWPORT_WIDTH})"
            )));
        }
        if !(MIN_VIEWPORT_DIM..=MAX_VIEWPORT_HEIGHT).contains(&height) {
            return Err(WebDriverError::InvalidScreenSize(format!(
                "height {height} is out of bounds ({MIN_VIEWPORT_DIM}..={MAX_VIEWPORT_HEIGHT})"
            )));
        }
        Ok(Self { width, height })
    }

    /// Parses `"<W>x<H>"` or `{ "width": W, "height": H }` with strict bounds validation.
    pub fn parse(value: &Value) -> Result<Self> {
        match value {
            Value::String(raw) => {
                let trimmed = raw.trim();
                let mut parts = trimmed.split(['x', 'X']);
                let Some(w_str) = parts.next() else {
                    return Err(WebDriverError::InvalidScreenSize(format!(
                        "expected \"<width>x<height>\" (got {raw:?})"
                    )));
                };
                let Some(h_str) = parts.next() else {
                    return Err(WebDriverError::InvalidScreenSize(format!(
                        "expected \"<width>x<height>\" (got {raw:?})"
                    )));
                };
                if parts.next().is_some()
                    || w_str.is_empty()
                    || h_str.is_empty()
                    || !w_str.bytes().all(|b| b.is_ascii_digit())
                    || !h_str.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(WebDriverError::InvalidScreenSize(format!(
                        "expected \"<width>x<height>\" with positive integers (got {raw:?})"
                    )));
                }
                let width = w_str.parse::<u32>().map_err(|_| {
                    WebDriverError::InvalidScreenSize(format!("width overflow in {raw:?}"))
                })?;
                let height = h_str.parse::<u32>().map_err(|_| {
                    WebDriverError::InvalidScreenSize(format!("height overflow in {raw:?}"))
                })?;
                Self::new(width, height)
            }
            Value::Object(map) => {
                for key in map.keys() {
                    if key != "width" && key != "height" {
                        return Err(WebDriverError::InvalidScreenSize(format!(
                            "unexpected field {key:?} in screenSize object (expected only `width` and `height`)"
                        )));
                    }
                }
                let width = parse_exact_u32(map.get("width"), "width")?;
                let height = parse_exact_u32(map.get("height"), "height")?;
                Self::new(width, height)
            }
            other => Err(WebDriverError::InvalidScreenSize(format!(
                "expected \"<width>x<height>\" string or {{\"width\": W, \"height\": H}} object (got {other})"
            ))),
        }
    }
}

fn parse_exact_u32(value: Option<&Value>, field: &str) -> Result<u32> {
    let Some(val) = value else {
        return Err(WebDriverError::InvalidScreenSize(format!(
            "missing `{field}` in screenSize object"
        )));
    };
    if let Some(u) = val.as_u64() {
        return u32::try_from(u).map_err(|_| {
            WebDriverError::InvalidScreenSize(format!("`{field}` {u} exceeds u32 range"))
        });
    }
    if let Some(f) = val.as_f64()
        && f.is_finite()
        && f >= 0.0
        && f.fract() == 0.0
        && f <= f64::from(u32::MAX)
    {
        return Ok(f as u32);
    }
    Err(WebDriverError::InvalidScreenSize(format!(
        "`{field}` must be a positive integer"
    )))
}

/// Strictly validates that `raw` is a loopback `http://` or `https://` URL targeting
/// `127.0.0.1`, `localhost`, or `[::1]`.
///
/// Rejects non-HTTP(S) schemes (`file://`, `javascript:`, `data:`, `chrome://`, etc.),
/// ASCII control or whitespace characters, `@` userinfo confusion, backslash authority smuggling,
/// invalid ports (`0` or `> 65535`), and non-loopback hosts.
pub fn validate_loopback_http_url(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Err(WebDriverError::InvalidUrl("URL must not be empty".into()));
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(WebDriverError::InvalidUrl(
            "URL must not contain control or whitespace characters".into(),
        ));
    }

    let rest = strip_ascii_prefix(raw, "http://")
        .or_else(|| strip_ascii_prefix(raw, "https://"))
        .ok_or_else(|| {
            WebDriverError::InvalidUrl(format!(
                "only loopback http:// or https:// URLs are permitted (got {raw:?})"
            ))
        })?;

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    validate_loopback_authority(authority, false, raw)?;
    Ok(raw.to_string())
}

/// Strictly validates a Chromium DevTools browser WebSocket URL emitted on Chromium stderr:
/// `ws://<loopback>:<1..=65535>/devtools/browser/<id>`.
pub(crate) fn validate_loopback_ws_url(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Err(WebDriverError::DevToolsUrlInvalid(
            "DevTools WebSocket URL is empty".into(),
        ));
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(WebDriverError::DevToolsUrlInvalid(
            "DevTools WebSocket URL contains control or whitespace characters".into(),
        ));
    }

    let rest = strip_ascii_prefix(raw, "ws://").ok_or_else(|| {
        WebDriverError::DevToolsUrlInvalid(format!(
            "DevTools URL must use ws:// scheme (got {raw:?})"
        ))
    })?;

    let Some(path_start) = rest.find('/') else {
        return Err(WebDriverError::DevToolsUrlInvalid(format!(
            "DevTools URL is missing /devtools/browser/ path (got {raw:?})"
        )));
    };
    let authority = &rest[..path_start];
    if authority.contains('?') || authority.contains('#') {
        return Err(WebDriverError::DevToolsUrlInvalid(format!(
            "DevTools URL has malformed authority (got {raw:?})"
        )));
    }
    validate_loopback_authority(authority, true, raw)
        .map_err(|err| WebDriverError::DevToolsUrlInvalid(err.to_string()))?;

    let path = &rest[path_start..];
    let Some(suffix) = path.strip_prefix("/devtools/browser/") else {
        return Err(WebDriverError::DevToolsUrlInvalid(format!(
            "DevTools URL path must start with /devtools/browser/ (got {path:?})"
        )));
    };
    if suffix.is_empty()
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'%'))
    {
        return Err(WebDriverError::DevToolsUrlInvalid(format!(
            "DevTools URL path suffix is empty or contains invalid characters (got {path:?})"
        )));
    }

    Ok(raw.to_string())
}

fn strip_ascii_prefix<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    let head = input.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&input[prefix.len()..])
    } else {
        None
    }
}

fn validate_loopback_authority(authority: &str, require_port: bool, raw: &str) -> Result<()> {
    if authority.is_empty() {
        return Err(WebDriverError::InvalidUrl(format!(
            "URL is missing host authority (got {raw:?})"
        )));
    }
    if authority.contains('@') {
        return Err(WebDriverError::InvalidUrl(format!(
            "URL must not contain userinfo `@` in authority (got {raw:?})"
        )));
    }
    if authority.contains('\\') {
        return Err(WebDriverError::InvalidUrl(format!(
            "URL must not contain backslash `\\` in authority (got {raw:?})"
        )));
    }

    let (host, port_opt) = if let Some(after_open) = authority.strip_prefix('[') {
        let Some(close_idx) = after_open.find(']') else {
            return Err(WebDriverError::InvalidUrl(format!(
                "unclosed IPv6 bracket in URL authority (got {raw:?})"
            )));
        };
        let host = &after_open[..close_idx];
        let remainder = &after_open[close_idx + 1..];
        let port_opt = if remainder.is_empty() {
            None
        } else if let Some(port_str) = remainder.strip_prefix(':') {
            Some(port_str)
        } else {
            return Err(WebDriverError::InvalidUrl(format!(
                "unexpected characters after IPv6 host in URL (got {raw:?})"
            )));
        };
        if !host.eq_ignore_ascii_case("::1") {
            return Err(WebDriverError::InvalidUrl(format!(
                "IPv6 host must be loopback [::1] (got [{host}])"
            )));
        }
        (host, port_opt)
    } else {
        let mut parts = authority.split(':');
        let host = parts.next().unwrap_or("");
        let port_opt = parts.next();
        if parts.next().is_some() {
            return Err(WebDriverError::InvalidUrl(format!(
                "malformed host:port authority in URL (got {raw:?})"
            )));
        }
        if !host.eq_ignore_ascii_case("127.0.0.1") && !host.eq_ignore_ascii_case("localhost") {
            return Err(WebDriverError::InvalidUrl(format!(
                "host must be loopback (127.0.0.1, localhost, or [::1]); got {host:?}"
            )));
        }
        (host, port_opt)
    };

    let _ = host;
    match port_opt {
        Some(port_str) => {
            if port_str.is_empty() || !port_str.bytes().all(|b| b.is_ascii_digit()) {
                return Err(WebDriverError::InvalidUrl(format!(
                    "invalid port {port_str:?} in URL (got {raw:?})"
                )));
            }
            let port = port_str.parse::<u16>().map_err(|_| {
                WebDriverError::InvalidUrl(format!(
                    "port {port_str:?} out of 1..=65535 range (got {raw:?})"
                ))
            })?;
            if port == 0 {
                return Err(WebDriverError::InvalidUrl(format!(
                    "port 0 is not permitted in URL (got {raw:?})"
                )));
            }
        }
        None if require_port => {
            return Err(WebDriverError::InvalidUrl(format!(
                "explicit port (1..=65535) is required (got {raw:?})"
            )));
        }
        None => {}
    }

    Ok(())
}

/// CDP `Input.dispatchKeyEvent` specification for one logical key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CdpKeySpec {
    pub key: String,
    pub code: String,
    pub windows_virtual_key_code: i32,
    pub text: Option<String>,
    pub unmodified_text: Option<String>,
    pub modifiers: u8,
}

/// Resolved action for [`KeyPress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedKeyAction {
    /// Navigate back in browser history (`pressKey: Back`).
    BrowserBack { count: u32 },
    /// Dispatch a CDP keyboard event `count` times.
    Dispatch { spec: CdpKeySpec, count: u32 },
}

/// Returns the standard CDP key spec for a plain `Backspace` key press (`eraseText`).
#[must_use]
pub(crate) fn backspace_key_spec() -> CdpKeySpec {
    CdpKeySpec {
        key: "Backspace".to_string(),
        code: "Backspace".to_string(),
        windows_virtual_key_code: 8,
        text: None,
        unmodified_text: None,
        modifiers: 0,
    }
}

/// Parses modifier strings into a CDP modifier bitmask, rejecting unknown modifiers fail-closed.
pub(crate) fn parse_modifiers(modifiers: &[String]) -> Result<u8> {
    let mut mask = 0u8;
    for raw in modifiers {
        match raw.trim().to_ascii_lowercase().as_str() {
            "alt" | "option" => mask |= MODIFIER_ALT,
            "ctrl" | "control" => mask |= MODIFIER_CTRL,
            "meta" | "cmd" | "command" | "super" => mask |= MODIFIER_META,
            "shift" => mask |= MODIFIER_SHIFT,
            _ => {
                return Err(WebDriverError::InvalidArgument(format!(
                    "unsupported key modifier {raw:?} (expected Alt, Ctrl, Meta, or Shift)"
                )));
            }
        }
    }
    Ok(mask)
}

/// Resolves a platform-neutral [`KeyPress`] into a CDP key event spec or browser back action.
pub(crate) fn resolve_key_press(press: &KeyPress) -> Result<ResolvedKeyAction> {
    if press.count > MAX_KEY_REPEAT_COUNT {
        return Err(WebDriverError::InvalidArgument(format!(
            "key repeat count {} exceeds maximum {MAX_KEY_REPEAT_COUNT}",
            press.count
        )));
    }
    let modifiers = parse_modifiers(&press.modifiers)?;
    let has_shortcut_mod = (modifiers & (MODIFIER_ALT | MODIFIER_CTRL | MODIFIER_META)) != 0;
    let raw_key = press.key.as_str();
    let trimmed = raw_key.trim();

    if trimmed.is_empty() && raw_key != " " {
        return Err(WebDriverError::InvalidArgument(
            "pressKey key name must not be empty".into(),
        ));
    }

    if trimmed.eq_ignore_ascii_case("back") {
        if modifiers != 0 {
            return Err(WebDriverError::InvalidArgument(
                "pressKey \"Back\" does not accept keyboard modifiers".into(),
            ));
        }
        return Ok(ResolvedKeyAction::BrowserBack { count: press.count });
    }

    let spec = match trimmed.to_ascii_lowercase().as_str() {
        "enter" | "return" => CdpKeySpec {
            key: "Enter".to_string(),
            code: "Enter".to_string(),
            windows_virtual_key_code: 13,
            text: (!has_shortcut_mod).then(|| "\r".to_string()),
            unmodified_text: (!has_shortcut_mod).then(|| "\r".to_string()),
            modifiers,
        },
        "tab" => CdpKeySpec {
            key: "Tab".to_string(),
            code: "Tab".to_string(),
            windows_virtual_key_code: 9,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "escape" | "esc" => CdpKeySpec {
            key: "Escape".to_string(),
            code: "Escape".to_string(),
            windows_virtual_key_code: 27,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "backspace" => CdpKeySpec {
            key: "Backspace".to_string(),
            code: "Backspace".to_string(),
            windows_virtual_key_code: 8,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "delete" | "forwarddelete" | "forward delete" => CdpKeySpec {
            key: "Delete".to_string(),
            code: "Delete".to_string(),
            windows_virtual_key_code: 46,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "space" | "" if raw_key == " " || trimmed.eq_ignore_ascii_case("space") => CdpKeySpec {
            key: " ".to_string(),
            code: "Space".to_string(),
            windows_virtual_key_code: 32,
            text: (!has_shortcut_mod).then(|| " ".to_string()),
            unmodified_text: (!has_shortcut_mod).then(|| " ".to_string()),
            modifiers,
        },
        "arrowup" | "up" => CdpKeySpec {
            key: "ArrowUp".to_string(),
            code: "ArrowUp".to_string(),
            windows_virtual_key_code: 38,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "arrowdown" | "down" => CdpKeySpec {
            key: "ArrowDown".to_string(),
            code: "ArrowDown".to_string(),
            windows_virtual_key_code: 40,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "arrowleft" | "left" => CdpKeySpec {
            key: "ArrowLeft".to_string(),
            code: "ArrowLeft".to_string(),
            windows_virtual_key_code: 37,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "arrowright" | "right" => CdpKeySpec {
            key: "ArrowRight".to_string(),
            code: "ArrowRight".to_string(),
            windows_virtual_key_code: 39,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "home" => CdpKeySpec {
            key: "Home".to_string(),
            code: "Home".to_string(),
            windows_virtual_key_code: 36,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "end" => CdpKeySpec {
            key: "End".to_string(),
            code: "End".to_string(),
            windows_virtual_key_code: 35,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "pageup" | "page up" => CdpKeySpec {
            key: "PageUp".to_string(),
            code: "PageUp".to_string(),
            windows_virtual_key_code: 33,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        "pagedown" | "page down" => CdpKeySpec {
            key: "PageDown".to_string(),
            code: "PageDown".to_string(),
            windows_virtual_key_code: 34,
            text: None,
            unmodified_text: None,
            modifiers,
        },
        _ => {
            let mut chars = raw_key.chars();
            let Some(ch) = chars.next() else {
                return Err(WebDriverError::InvalidArgument(
                    "pressKey key name must not be empty".into(),
                ));
            };
            if chars.next().is_some() || ch.is_control() {
                return Err(WebDriverError::InvalidArgument(format!(
                    "unsupported web key {raw_key:?}"
                )));
            }
            single_char_key_spec(ch, modifiers, has_shortcut_mod)
        }
    };

    Ok(ResolvedKeyAction::Dispatch {
        spec,
        count: press.count,
    })
}

fn single_char_key_spec(ch: char, modifiers: u8, has_shortcut_mod: bool) -> CdpKeySpec {
    let shift = (modifiers & MODIFIER_SHIFT) != 0;
    let effective_char = if shift && ch.is_ascii_lowercase() {
        ch.to_ascii_uppercase()
    } else {
        ch
    };
    let (code, vk) = match ch {
        'a'..='z' | 'A'..='Z' => {
            let upper = ch.to_ascii_uppercase();
            (format!("Key{upper}"), i32::from(upper as u8))
        }
        '0'..='9' => (format!("Digit{ch}"), i32::from(ch as u8)),
        '-' | '_' => ("Minus".to_string(), 189),
        '=' | '+' => ("Equal".to_string(), 187),
        '[' | '{' => ("BracketLeft".to_string(), 219),
        ']' | '}' => ("BracketRight".to_string(), 221),
        '\\' | '|' => ("Backslash".to_string(), 220),
        ';' | ':' => ("Semicolon".to_string(), 186),
        '\'' | '"' => ("Quote".to_string(), 222),
        ',' | '<' => ("Comma".to_string(), 188),
        '.' | '>' => ("Period".to_string(), 190),
        '/' | '?' => ("Slash".to_string(), 191),
        '`' | '~' => ("Backquote".to_string(), 192),
        _ => (String::new(), 0),
    };

    let key_str = effective_char.to_string();
    let unmodified = ch.to_string();
    CdpKeySpec {
        key: key_str.clone(),
        code,
        windows_virtual_key_code: vk,
        text: (!has_shortcut_mod).then_some(key_str),
        unmodified_text: (!has_shortcut_mod).then_some(unmodified),
        modifiers,
    }
}
