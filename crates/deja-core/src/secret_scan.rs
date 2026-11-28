//! Fixture credential scanning across raw text, YAML decoded scalars, JSON strings, and binary media.

use std::collections::HashSet;

use crate::error::CoreError;
use crate::flow_scalars::find_flow_strings;

/// Shortest fixture credential value allowed for reliable scanning (matches TypeScript `MIN_FORBIDDEN_VALUE_LENGTH`).
pub const MIN_FORBIDDEN_VALUE_LENGTH: usize = 3;

/// Fail closed if any configured forbidden fixture value is shorter than [`MIN_FORBIDDEN_VALUE_LENGTH`].
pub fn validate_forbidden_values(values: &[String]) -> Result<(), CoreError> {
    if values
        .iter()
        .any(|value| value.encode_utf16().count() < MIN_FORBIDDEN_VALUE_LENGTH)
    {
        return Err(CoreError::ForbiddenValueTooShort {
            min_length: MIN_FORBIDDEN_VALUE_LENGTH,
        });
    }
    Ok(())
}

fn is_uri_component_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
        )
}

/// Exact ECMAScript `encodeURIComponent` encoding for valid UTF-8 strings.
fn encode_uri_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if is_uri_component_unreserved(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// Standard RFC 4648 base64 encoding with `=` padding (matches Node `Buffer.from(v).toString('base64')`).
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Encoded spellings a forbidden fixture value could take inside YAML, JSON, URLs, or base64 blobs.
#[must_use]
pub fn spellings(value: &str) -> Vec<String> {
    let serialized = serde_json::to_string(value).expect("UTF-8 string serializes");
    let json_inner = &serialized[1..serialized.len() - 1];
    let candidates = [
        value.to_string(),
        json_inner.to_string(),
        json_inner.replace('/', "\\/"),
        value.replace('\'', "''"),
        encode_uri_component(value),
        base64_encode(value.as_bytes()),
    ];
    let mut seen = HashSet::with_capacity(candidates.len());
    let mut unique = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if seen.insert(candidate.clone()) {
            unique.push(candidate);
        }
    }
    unique
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Returns `true` if `text` contains any forbidden fixture value in raw, encoded, or decoded YAML form.
#[must_use]
pub fn holds_forbidden_value(text: &str, values: &[String]) -> bool {
    if values.is_empty() {
        return false;
    }
    let decoded = find_flow_strings(text).unwrap_or_default();
    values.iter().any(|value| {
        spellings(value)
            .iter()
            .any(|form| text.contains(form.as_str()))
            || decoded.iter().any(|item| item.contains(value.as_str()))
    })
}

/// Returns `true` if binary `bytes` (e.g. `addMedia` files) contain any encoded spelling of a forbidden value.
#[must_use]
pub fn bytes_hold_forbidden_value(bytes: &[u8], values: &[String]) -> bool {
    if values.is_empty() {
        return false;
    }
    values.iter().any(|value| {
        spellings(value)
            .iter()
            .any(|form| contains_subslice(bytes, form.as_bytes()))
    })
}

fn parse_hex4(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 4 {
        return None;
    }
    let hex_str = std::str::from_utf8(&bytes[..4]).ok()?;
    u16::from_str_radix(hex_str, 16).ok()
}

/// Lenient fallback JSON escape decoder used if a string token contains a lone surrogate or
/// truncated escape sequence so valid `\uXXXX` escapes next to it are still decoded.
fn unescape_json_str_lenient(inner: &str) -> String {
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'"' => {
                    out.push('"');
                    i += 2;
                }
                b'\\' => {
                    out.push('\\');
                    i += 2;
                }
                b'/' => {
                    out.push('/');
                    i += 2;
                }
                b'b' => {
                    out.push('\u{0008}');
                    i += 2;
                }
                b'f' => {
                    out.push('\u{000C}');
                    i += 2;
                }
                b'n' => {
                    out.push('\n');
                    i += 2;
                }
                b'r' => {
                    out.push('\r');
                    i += 2;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                }
                b'u' => {
                    if let Some(code1) = parse_hex4(&bytes[i + 2..]) {
                        if (0xD800..=0xDBFF).contains(&code1)
                            && i + 12 <= bytes.len()
                            && bytes[i + 6] == b'\\'
                            && bytes[i + 7] == b'u'
                            && let Some(code2) = parse_hex4(&bytes[i + 8..])
                            && (0xDC00..=0xDFFF).contains(&code2)
                        {
                            let cp = 0x10000
                                + (((u32::from(code1) - 0xD800) << 10)
                                    | (u32::from(code2) - 0xDC00));
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            i += 12;
                            continue;
                        }
                        out.push(char::from_u32(u32::from(code1)).unwrap_or('\u{FFFD}'));
                        i += 6;
                    } else {
                        out.push('\\');
                        i += 1;
                    }
                }
                _ => {
                    out.push('\\');
                    i += 1;
                }
            }
        } else if let Some(ch) = inner[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

/// Extract every decoded JSON string literal iteratively in linear time with zero recursion limit.
///
/// Handles arbitrarily deeply nested JSON documents (`>128` levels) without tripping `serde_json`'s
/// container recursion limit.
fn scan_json_string_tokens(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut strings = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut closed = false;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    i = (i + 2).min(bytes.len());
                }
                b'"' => {
                    i += 1;
                    closed = true;
                    break;
                }
                _ => i += 1,
            }
        }
        if closed {
            let token = &text[start..i];
            if let Ok(decoded) = serde_json::from_str::<String>(token) {
                strings.push(decoded);
            } else {
                strings.push(unescape_json_str_lenient(&text[start + 1..i - 1]));
            }
        } else {
            strings.push(unescape_json_str_lenient(&text[start + 1..]));
        }
    }

    strings
}

/// Returns `true` if a JSON document holds any forbidden value in raw, encoded, or decoded string form.
#[must_use]
pub fn json_holds_forbidden_value(text: &str, values: &[String]) -> bool {
    if values.is_empty() {
        return false;
    }
    let strings = scan_json_string_tokens(text);
    values.iter().any(|value| {
        spellings(value)
            .iter()
            .any(|form| text.contains(form.as_str()))
            || strings.iter().any(|item| item.contains(value.as_str()))
    })
}
