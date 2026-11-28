//! Linear-time path, fixture, JSON-field, environment secret, URL password, and provider key redaction.

use std::borrow::Cow;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::contracts::is_js_whitespace;

/// Shortest value hidden for password-like environment variable names.
pub const PASSWORD_FLOOR: usize = 4;

/// Shortest value hidden for key/token/session-like environment variable names.
pub const KEY_FLOOR: usize = 8;

const SECRET_WORDS: &[&str] = &[
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "TOKEN",
    "APIKEY",
    "CREDENTIAL",
    "PRIVATEKEY",
    "ACCESSKEY",
];

const SECRET_PARTS: &[&str] = &[
    "KEY", "PASS", "PWD", "AUTH", "SESSION", "COOKIE", "PAT", "JWT", "PRIVATE", "SK",
];

const SECRET_PREFIXES: &[&str] = &[
    "API",
    "SECRET",
    "ACCESS",
    "PRIVATE",
    "DB",
    "SSH",
    "AUTH",
    "SIGNING",
    "ENCRYPTION",
    "MASTER",
    "PG",
];

fn camel_split_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([a-z0-9])([A-Z])").expect("valid camel split regex"))
}

fn name_sep_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[_.-]+").expect("valid separator regex"))
}

fn json_secret_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:password|secret|token|credential)").expect("valid secret key regex")
    })
}

fn sk_pk_rk_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:sk|pk|rk)[-_][A-Za-z0-9_-]{16,}").expect("valid sk/pk/rk regex")
    })
}

fn gh_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"gh[pousr]_[A-Za-z0-9]{20,}").expect("valid gh token regex"))
}

fn github_pat_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"github_pat_[A-Za-z0-9_]{20,}").expect("valid github pat regex"))
}

fn aws_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:AKIA|ASIA)[A-Z0-9]{16}").expect("valid aws key regex"))
}

fn slack_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"xox[abeprs]-[A-Za-z0-9-]{10,}").expect("valid slack token regex")
    })
}

fn bearer_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)Bearer[\t\n\x0B\x0C\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]+[A-Za-z0-9._~+/=-]{20,}",
        )
        .expect("valid bearer regex")
    })
}

/// Minimum value length to hide for an environment variable name, or `None` for non-secret names.
///
/// Mirrors TypeScript `secretFloor`: splits on `_`, `-`, `.`, and camelCase; strips trailing digits;
/// ignores shell `PWD`/`OLDPWD` and ordinary words like `KEYBOARD`, `MONKEY`, `COMPASS`, `AUTHOR`.
#[must_use]
pub fn secret_floor(name: &str) -> Option<usize> {
    if name == "PWD" || name == "OLDPWD" {
        return None;
    }
    let camel_expanded = camel_split_regex().replace_all(name, "${1}_${2}");
    let parts: Vec<String> = name_sep_regex()
        .split(&camel_expanded)
        .map(|part| {
            let upper = part.to_ascii_uppercase();
            upper
                .trim_end_matches(|c: char| c.is_ascii_digit())
                .to_string()
        })
        .filter(|part| !part.is_empty())
        .collect();

    let joined = parts.concat();
    let mut found: Option<usize> = None;
    let mut note = |floor: usize| {
        found = Some(match found {
            Some(prev) => prev.min(floor),
            None => floor,
        });
    };

    for &word in SECRET_WORDS {
        if joined.contains(word) {
            note(if word == "PASSWORD" || word == "PASSWD" {
                PASSWORD_FLOOR
            } else {
                KEY_FLOOR
            });
        }
    }

    for part in &parts {
        if SECRET_PARTS.contains(&part.as_str()) {
            note(if part == "PASS" || part == "PWD" {
                PASSWORD_FLOOR
            } else {
                KEY_FLOOR
            });
        }
        for suffix in ["KEY", "PASS"] {
            if part.len() > suffix.len()
                && let Some(prefix) = part.strip_suffix(suffix)
                && SECRET_PREFIXES.contains(&prefix)
            {
                note(if suffix == "PASS" {
                    PASSWORD_FLOOR
                } else {
                    KEY_FLOOR
                });
            }
        }
    }

    found
}

/// Returns `true` when an environment variable name looks like a secret or credential.
#[must_use]
pub fn is_key_like_name(name: &str) -> bool {
    secret_floor(name).is_some()
}

/// Deterministic substring redactor for workspace roots, home directories, and fixture values.
#[derive(Debug, Clone)]
pub struct Redactor {
    replacements: Vec<(String, &'static str)>,
}

impl Redactor {
    /// Apply every replacement in order to `text`.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (search, replacement) in &self.replacements {
            if !search.is_empty() {
                out = out.replace(search.as_str(), replacement);
            }
        }
        out
    }
}

/// Build a [`Redactor`] replacing `workspace_root`, optional `home_dir`, and non-empty `fixture_values`.
#[must_use]
pub fn create_redactor(
    workspace_root: &str,
    home_dir: Option<&str>,
    fixture_values: &[String],
) -> Redactor {
    let mut replacements: Vec<(String, &'static str)> = Vec::new();
    if !workspace_root.is_empty() {
        replacements.push((workspace_root.to_string(), "<workspace>"));
    }
    if let Some(home) = home_dir
        && !home.is_empty()
    {
        replacements.push((home.to_string(), "<home>"));
    }
    for value in fixture_values {
        if !value.is_empty() {
            replacements.push((value.clone(), "<redacted-fixture-value>"));
        }
    }
    Redactor { replacements }
}

/// Recursively redact strings and secret-named object fields in a JSON [`Value`].
#[must_use]
pub fn redact_json(value: &Value, redactor: &Redactor) -> Value {
    match value {
        Value::String(s) => Value::String(redactor.redact(s)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| redact_json(v, redactor)).collect())
        }
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, entry) in map {
                if json_secret_key_regex().is_match(key) && entry.is_string() {
                    out.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else {
                    out.insert(key.clone(), redact_json(entry, redactor));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn is_scheme_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-')
}

fn is_token_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn is_upper_alnum(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit()
}

fn is_ascii_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Single-pass linear-time URL password redactor matching:
/// `/(?<![A-Za-z0-9+.-])([A-Za-z][A-Za-z0-9+.-]{0,31}:\/\/[^\s:/?#@]{0,256}:)[^\s/?#]{1,1024}(?=@)/g`
fn redact_url_passwords(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    let mut out: Option<String> = None;
    let mut last_emit = 0_usize;
    let mut search_from = 0_usize;

    while let Some(rel_sep) = input[search_from..].find("://") {
        let sep = search_from + rel_sep;

        // Walk backward over contiguous scheme characters; cannot cross the previous `://` because `:` stops it.
        let mut scheme_start = sep;
        while scheme_start > 0 && is_scheme_char(bytes[scheme_start - 1]) {
            scheme_start -= 1;
        }
        let scheme_len = sep - scheme_start;
        if !(1..=32).contains(&scheme_len) || !bytes[scheme_start].is_ascii_alphabetic() {
            search_from = sep + 3;
            continue;
        }

        // Scan up to 256 user characters stopping at whitespace or `:/?#@`.
        let user_start = sep + 3;
        let mut colon_pos: Option<usize> = None;
        for (user_chars, (rel_idx, ch)) in input[user_start..].char_indices().enumerate() {
            if ch == ':' {
                colon_pos = Some(user_start + rel_idx);
                break;
            }
            if user_chars >= 256 || is_js_whitespace(ch) || matches!(ch, '/' | '?' | '#' | '@') {
                break;
            }
        }

        let Some(colon) = colon_pos else {
            search_from = sep + 3;
            continue;
        };

        // Scan up to 1024 password characters for the last `@` before whitespace or `/?#`.
        let pw_start = colon + 1;
        let mut last_at: Option<usize> = None;
        for (pw_chars, (rel_idx, ch)) in input[pw_start..].char_indices().enumerate() {
            if ch == '@' && (1..=1024).contains(&pw_chars) {
                last_at = Some(pw_start + rel_idx);
            }
            if pw_chars >= 1024 || is_js_whitespace(ch) || matches!(ch, '/' | '?' | '#') {
                break;
            }
        }

        if let Some(at_pos) = last_at {
            let buf = out.get_or_insert_with(|| String::with_capacity(input.len()));
            buf.push_str(&input[last_emit..pw_start]);
            buf.push_str("<redacted-secret>");
            last_emit = at_pos;
            search_from = at_pos;
        } else {
            search_from = sep + 3;
        }
    }

    match out {
        Some(mut buf) => {
            buf.push_str(&input[last_emit..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(input),
    }
}

fn replace_with_boundaries<'a>(
    input: &'a str,
    regex: &Regex,
    disallow_before: fn(u8) -> bool,
    disallow_after: Option<fn(u8) -> bool>,
    replacement: &str,
) -> Cow<'a, str> {
    let bytes = input.as_bytes();
    let mut out: Option<String> = None;
    let mut last_emit = 0_usize;

    for matched in regex.find_iter(input) {
        let start = matched.start();
        let end = matched.end();
        let blocked_before = start > 0 && disallow_before(bytes[start - 1]);
        let blocked_after =
            disallow_after.is_some_and(|pred| end < bytes.len() && pred(bytes[end]));
        if blocked_before || blocked_after {
            continue;
        }
        let buf = out.get_or_insert_with(|| String::with_capacity(input.len()));
        buf.push_str(&input[last_emit..start]);
        buf.push_str(replacement);
        last_emit = end;
    }

    match out {
        Some(mut buf) => {
            buf.push_str(&input[last_emit..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(input),
    }
}

/// Redacts values of key-like environment variables plus URL passwords and provider token shapes.
#[derive(Debug, Clone)]
pub struct ProviderKeyRedactor {
    env_values: Vec<String>,
}

impl ProviderKeyRedactor {
    /// Redact environment secret values, URL passwords, and provider key shapes in linear time.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let mut redacted: Cow<'_, str> = Cow::Borrowed(text);
        for value in &self.env_values {
            if redacted.contains(value.as_str()) {
                redacted = Cow::Owned(redacted.replace(value.as_str(), "<redacted-secret>"));
            }
        }
        if let Cow::Owned(next) = redact_url_passwords(redacted.as_ref()) {
            redacted = Cow::Owned(next);
        }
        if let Cow::Owned(next) = replace_with_boundaries(
            redacted.as_ref(),
            sk_pk_rk_regex(),
            is_token_word_char,
            None,
            "<redacted-secret>",
        ) {
            redacted = Cow::Owned(next);
        }
        if let Cow::Owned(next) = replace_with_boundaries(
            redacted.as_ref(),
            gh_token_regex(),
            is_token_word_char,
            None,
            "<redacted-secret>",
        ) {
            redacted = Cow::Owned(next);
        }
        if let Cow::Owned(next) = replace_with_boundaries(
            redacted.as_ref(),
            github_pat_regex(),
            is_token_word_char,
            None,
            "<redacted-secret>",
        ) {
            redacted = Cow::Owned(next);
        }
        if let Cow::Owned(next) = replace_with_boundaries(
            redacted.as_ref(),
            aws_key_regex(),
            is_upper_alnum,
            Some(is_upper_alnum),
            "<redacted-secret>",
        ) {
            redacted = Cow::Owned(next);
        }
        if let Cow::Owned(next) = replace_with_boundaries(
            redacted.as_ref(),
            slack_token_regex(),
            is_token_word_char,
            None,
            "<redacted-secret>",
        ) {
            redacted = Cow::Owned(next);
        }
        if let Cow::Owned(next) = replace_with_boundaries(
            redacted.as_ref(),
            bearer_regex(),
            is_ascii_word_byte,
            None,
            "Bearer <redacted-secret>",
        ) {
            redacted = Cow::Owned(next);
        }
        redacted.into_owned()
    }
}

/// Construct a [`ProviderKeyRedactor`] from an iterator of `(name, value)` environment pairs.
#[must_use]
pub fn provider_key_redactor<I, K, V>(env: I) -> ProviderKeyRedactor
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut env_values: Vec<String> = env
        .into_iter()
        .filter_map(|(name, value)| {
            let floor = secret_floor(name.as_ref())?;
            let val = value.as_ref();
            if val.encode_utf16().count() >= floor {
                Some(val.to_string())
            } else {
                None
            }
        })
        .collect();
    env_values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    ProviderKeyRedactor { env_values }
}
