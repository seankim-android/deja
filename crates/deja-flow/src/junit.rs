//! Pure-std redacted JUnit XML writer, entity-decoded redactor, atomic fallback writer,
//! and multi-suite merger with parity to legacy `apps/cli/src/junit.ts`.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use deja_core::{CrashSignalSource, FailureCategory, ProviderKeyRedactor, Redactor, RunAttempt};

/// Attributes whose numeric/status values must never be altered by redaction so CI parsers
/// always read intact test counts and statuses.
const STRUCTURAL_ATTRIBUTES: &[&str] =
    &["status", "failures", "errors", "tests", "skipped", "time"];

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns `true` when `ch` is permitted in XML 1.0 character data (`Char` production).
fn is_valid_xml_1_0_char(ch: char) -> bool {
    let code = ch as u32;
    matches!(code, 0x09 | 0x0A | 0x0D | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

/// Strips characters forbidden by XML 1.0 (e.g., NUL and C0 controls other than TAB/LF/CR).
#[must_use]
pub fn strip_illegal_xml_chars(text: &str) -> String {
    text.chars().filter(|&c| is_valid_xml_1_0_char(c)).collect()
}

/// Strips XML 1.0 illegal control characters and escapes `&`, `<`, `>`, `"`, and `'`.
#[must_use]
pub fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if !is_valid_xml_1_0_char(ch) {
            continue;
        }
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Sanitizes content destined for `<![CDATA[...]]>`: strips illegal XML 1.0 control chars
/// and splits any `]]>` sequence into `]]]]><![CDATA[>` so content can never break out of CDATA.
#[must_use]
pub fn escape_cdata(text: &str) -> String {
    strip_illegal_xml_chars(text).replace("]]>", "]]]]><![CDATA[>")
}

/// Sanitizes XML comment body: strips illegal XML 1.0 chars and breaks forbidden `--` runs.
fn sanitize_comment(text: &str) -> String {
    let mut cleaned = strip_illegal_xml_chars(text);
    while cleaned.contains("--") {
        cleaned = cleaned.replace("--", "- -");
    }
    if cleaned.ends_with('-') {
        cleaned.push(' ');
    }
    cleaned
}

/// Decodes numeric (`&#x...;`, `&#...;`) and named (`&lt;`, `&gt;`, `&quot;`, `&apos;`, `&amp;`)
/// XML entities in a single pass so entity-encoded secrets cannot bypass substring redaction.
#[must_use]
pub fn decode_xml(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(rel_semi) = text[i + 1..].find(';')
        {
            let entity = &text[i + 1..i + 1 + rel_semi];
            let decoded: Option<Option<char>> = if let Some(hex) = entity.strip_prefix("#x") {
                if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    let cp = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
                    Some(cp)
                } else {
                    None
                }
            } else if let Some(dec) = entity.strip_prefix('#') {
                if !dec.is_empty() && dec.bytes().all(|b| b.is_ascii_digit()) {
                    let cp = dec.parse::<u32>().ok().and_then(char::from_u32);
                    Some(cp)
                } else {
                    None
                }
            } else {
                match entity {
                    "lt" => Some(Some('<')),
                    "gt" => Some(Some('>')),
                    "quot" => Some(Some('"')),
                    "apos" => Some(Some('\'')),
                    "amp" => Some(Some('&')),
                    _ => None,
                }
            };

            if let Some(maybe_ch) = decoded {
                if let Some(ch) = maybe_ch {
                    out.push(ch);
                }
                i += rel_semi + 2;
                continue;
            }
        }

        let ch = text[i..].chars().next().expect("valid UTF-8 char");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

fn collapse_whitespace_and_truncate(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(max_chars).collect()
}

fn category_wire_name(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::Selector => "selector",
        FailureCategory::Navigation => "navigation",
        FailureCategory::ProductAssertion => "product_assertion",
        FailureCategory::Runner => "runner",
        FailureCategory::Timeout => "timeout",
        FailureCategory::Unknown => "unknown",
        FailureCategory::AppCrash => "app_crash",
    }
}

fn crash_source_wire_name(src: CrashSignalSource) -> &'static str {
    match src {
        CrashSignalSource::MaestroOutput => "maestro-output",
        CrashSignalSource::CrashReport => "crash-report",
    }
}

/// Input for [`synthesized_junit`].
#[derive(Debug, Clone, Copy)]
pub struct SynthesizedJunitInput<'a> {
    pub goal: &'a str,
    pub passed: bool,
    pub reason: &'a str,
    pub duration_ms: u64,
}

/// Synthesizes a minimal well-formed JUnit report for a single run, tagged with
/// `<property name="source" value="deja-synthesized"/>`.
#[must_use]
pub fn synthesized_junit(input: &SynthesizedJunitInput<'_>) -> String {
    let goal = escape_xml(input.goal);
    let seconds = format!("{:.3}", (input.duration_ms as f64) / 1000.0);
    let testcase = if input.passed {
        format!(
            "<testcase id=\"{goal}\" name=\"{goal}\" classname=\"{goal}\" time=\"{seconds}\" status=\"SUCCESS\"/>"
        )
    } else {
        let short_msg = escape_xml(&collapse_whitespace_and_truncate(input.reason, 300));
        let body = escape_xml(input.reason);
        format!(
            "<testcase id=\"{goal}\" name=\"{goal}\" classname=\"{goal}\" time=\"{seconds}\" status=\"ERROR\">\n      <failure message=\"{short_msg}\">{body}</failure>\n    </testcase>"
        )
    };
    let failures = if input.passed { 0 } else { 1 };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n  <testsuite name=\"Test Suite\" tests=\"1\" failures=\"{failures}\" time=\"{seconds}\">\n    <properties>\n      <property name=\"source\" value=\"deja-synthesized\"/>\n    </properties>\n    {testcase}\n  </testsuite>\n</testsuites>\n"
    )
}

/// Input for [`synthesized_attempts_junit`].
#[derive(Debug, Clone, Copy)]
pub struct SynthesizedAttemptsJunitInput<'a> {
    pub goal: &'a str,
    pub outcome: &'a str,
    pub attempts: &'a [RunAttempt],
    pub duration_ms: u64,
}

/// Synthesizes a multi-attempt JUnit report (e.g. `--rerun-failed`), emitting one `<testcase>`
/// per attempt so CI hosts always see both attempts and never mistake a flaky run for a pass.
#[must_use]
pub fn synthesized_attempts_junit(input: &SynthesizedAttemptsJunitInput<'_>) -> String {
    let goal = escape_xml(input.goal);
    let failures = input.attempts.iter().filter(|a| !a.passed).count();
    let mut cases = Vec::with_capacity(input.attempts.len());

    for attempt in input.attempts {
        let name = format!("{goal} (attempt {})", attempt.attempt);
        let seconds = format!("{:.3}", (attempt.duration_ms as f64) / 1000.0);
        if attempt.passed {
            cases.push(format!(
                "<testcase id=\"{name}\" name=\"{name}\" classname=\"{goal}\" time=\"{seconds}\" status=\"SUCCESS\"/>"
            ));
        } else {
            let mut parts = vec![
                format!("outcome: {}", input.outcome),
                format!(
                    "label: {}",
                    attempt
                        .failure
                        .as_ref()
                        .map(|f| category_wire_name(f.category))
                        .unwrap_or("unclassified")
                ),
            ];
            if let Some(ref failure) = attempt.failure {
                if let Some(ref step) = failure.failing_step {
                    parts.push(format!("step: {step}"));
                }
                if let Some(ref artifact) = failure.raw_artifact_path {
                    parts.push(format!("artifact: {artifact}"));
                }
                if let Some(ref crash) = failure.crash {
                    let report_suffix = crash
                        .report_file
                        .as_deref()
                        .map(|f| format!(" {f}"))
                        .unwrap_or_default();
                    parts.push(format!(
                        "crash signal: {}{}",
                        crash_source_wire_name(crash.source),
                        report_suffix
                    ));
                }
            }
            let detail = parts.join("; ");
            let short_msg = escape_xml(&collapse_whitespace_and_truncate(&detail, 300));
            let body = escape_xml(&detail);
            cases.push(format!(
                "<testcase id=\"{name}\" name=\"{name}\" classname=\"{goal}\" time=\"{seconds}\" status=\"ERROR\">\n      <failure message=\"{short_msg}\">{body}</failure>\n    </testcase>"
            ));
        }
    }

    let seconds = format!("{:.3}", (input.duration_ms as f64) / 1000.0);
    let outcome = escape_xml(input.outcome);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n  <testsuite name=\"Test Suite\" tests=\"{}\" failures=\"{failures}\" time=\"{seconds}\">\n    <properties>\n      <property name=\"source\" value=\"deja-synthesized\"/>\n      <property name=\"outcome\" value=\"{outcome}\"/>\n    </properties>\n    {}\n  </testsuite>\n</testsuites>\n",
        input.attempts.len(),
        cases.join("\n    ")
    )
}

/// Compact per-attempt breakdown included in a batch JUnit `<failure>` body when a goal ran
/// multiple attempts (`--rerun-failed` / `--retries`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchJunitAttemptSummary {
    pub attempt: u8,
    pub passed: bool,
    pub duration_ms: u64,
    pub label: Option<String>,
    pub failing_step: Option<String>,
    pub artifact_path: Option<String>,
    pub crash_summary: Option<String>,
}

impl From<&RunAttempt> for BatchJunitAttemptSummary {
    fn from(attempt: &RunAttempt) -> Self {
        let (label, failing_step, artifact_path, crash_summary) = match attempt.failure.as_ref() {
            Some(failure) => {
                let crash = failure.crash.as_ref().map(|c| {
                    let report_suffix = c
                        .report_file
                        .as_deref()
                        .map(|f| format!(" {f}"))
                        .unwrap_or_default();
                    format!("{}{}", crash_source_wire_name(c.source), report_suffix)
                });
                (
                    Some(category_wire_name(failure.category).to_string()),
                    failure.failing_step.clone(),
                    failure.raw_artifact_path.clone(),
                    crash,
                )
            }
            None => (None, None, None, None),
        };
        Self {
            attempt: attempt.attempt,
            passed: attempt.passed,
            duration_ms: attempt.duration_ms,
            label,
            failing_step,
            artifact_path,
            crash_summary,
        }
    }
}

/// Outcome category for one `<testcase>` inside a merged batch JUnit report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchJunitCaseStatus {
    Passed,
    Failed,
    Errored,
    Skipped,
}

/// One goal entry in a merged batch JUnit report (emitted in deterministic input order `0..N`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchJunitCase {
    pub goal: String,
    pub status: BatchJunitCaseStatus,
    pub outcome: String,
    pub duration_ms: u64,
    pub detail: Option<String>,
    pub device: Option<String>,
    pub error_type: Option<String>,
    pub attempts: Vec<BatchJunitAttemptSummary>,
}

fn format_batch_attempt_summary(attempt: &BatchJunitAttemptSummary) -> String {
    if attempt.passed {
        format!(
            "attempt {}: passed ({}ms)",
            attempt.attempt, attempt.duration_ms
        )
    } else {
        let mut parts = vec![format!(
            "label: {}",
            attempt.label.as_deref().unwrap_or("unclassified")
        )];
        if let Some(ref step) = attempt.failing_step {
            parts.push(format!("step: {step}"));
        }
        if let Some(ref artifact) = attempt.artifact_path {
            parts.push(format!("artifact: {artifact}"));
        }
        if let Some(ref crash) = attempt.crash_summary {
            parts.push(format!("crash signal: {crash}"));
        }
        parts.push(format!("{}ms", attempt.duration_ms));
        format!("attempt {}: failed ({})", attempt.attempt, parts.join(", "))
    }
}

/// Synthesizes a merged multi-goal batch JUnit report containing a single `<testsuite>` tagged
/// with `<property name="source" value="deja-synthesized"/>`, `mode="batch"`, and `outcome`,
/// emitting one `<testcase>` per goal in deterministic input order `0..N` and wrapping through
/// [`merge_junit_suites`] so `<testsuites>` and `<testsuite>` aggregate counts match accurately.
#[must_use]
pub fn synthesized_batch_junit(
    suite_name: &str,
    outcome: &str,
    cases: &[BatchJunitCase],
    duration_ms: u64,
) -> String {
    let mut failures: u64 = 0;
    let mut errors: u64 = 0;
    let mut skipped: u64 = 0;
    let mut rendered_cases = Vec::with_capacity(cases.len());

    for case in cases {
        let goal = escape_xml(&case.goal);
        let seconds = format!("{:.3}", (case.duration_ms as f64) / 1000.0);
        match case.status {
            BatchJunitCaseStatus::Passed => {
                rendered_cases.push(format!(
                    "  <testcase id=\"{goal}\" name=\"{goal}\" classname=\"{goal}\" time=\"{seconds}\" status=\"SUCCESS\"/>"
                ));
            }
            BatchJunitCaseStatus::Failed => {
                failures = failures.saturating_add(1);
                let mut parts = vec![format!("outcome: {}", case.outcome)];
                if let Some(ref device) = case.device
                    && !device.is_empty()
                {
                    parts.push(format!("device: {device}"));
                }
                if let Some(ref detail) = case.detail
                    && !detail.is_empty()
                {
                    parts.push(detail.clone());
                }
                if case.attempts.len() >= 2 {
                    let breakdown = case
                        .attempts
                        .iter()
                        .map(format_batch_attempt_summary)
                        .collect::<Vec<_>>()
                        .join("; ");
                    parts.push(format!("attempts ({}): {breakdown}", case.attempts.len()));
                }
                let full_detail = parts.join("; ");
                let short_msg = escape_xml(&collapse_whitespace_and_truncate(&full_detail, 300));
                let body = escape_xml(&full_detail);
                rendered_cases.push(format!(
                    "  <testcase id=\"{goal}\" name=\"{goal}\" classname=\"{goal}\" time=\"{seconds}\" status=\"ERROR\">\n    <failure message=\"{short_msg}\">{body}</failure>\n  </testcase>"
                ));
            }
            BatchJunitCaseStatus::Errored => {
                errors = errors.saturating_add(1);
                let err_type = escape_xml(case.error_type.as_deref().unwrap_or(
                    if case.outcome == "cancelled" {
                        "Cancelled"
                    } else {
                        "Error"
                    },
                ));
                let mut parts = vec![format!("outcome: {}", case.outcome)];
                if let Some(ref device) = case.device
                    && !device.is_empty()
                {
                    parts.push(format!("device: {device}"));
                }
                if let Some(ref detail) = case.detail
                    && !detail.is_empty()
                {
                    parts.push(detail.clone());
                }
                let full_detail = parts.join("; ");
                let short_msg = escape_xml(&collapse_whitespace_and_truncate(&full_detail, 300));
                let body = escape_xml(&full_detail);
                rendered_cases.push(format!(
                    "  <testcase id=\"{goal}\" name=\"{goal}\" classname=\"{goal}\" time=\"{seconds}\" status=\"ERROR\">\n    <error type=\"{err_type}\" message=\"{short_msg}\">{body}</error>\n  </testcase>"
                ));
            }
            BatchJunitCaseStatus::Skipped => {
                skipped = skipped.saturating_add(1);
                let skip_text = match case.detail.as_deref() {
                    Some(d) if !d.is_empty() => {
                        if d.starts_with("cancelled before execution") {
                            d.to_string()
                        } else {
                            format!("cancelled before execution: {d}")
                        }
                    }
                    _ => "cancelled before execution".to_string(),
                };
                let short_msg = escape_xml(&collapse_whitespace_and_truncate(&skip_text, 300));
                rendered_cases.push(format!(
                    "  <testcase id=\"{goal}\" name=\"{goal}\" classname=\"{goal}\" time=\"{seconds}\" status=\"SKIPPED\">\n    <skipped message=\"{short_msg}\"/>\n  </testcase>"
                ));
            }
        }
    }

    let escaped_suite = escape_xml(suite_name);
    let escaped_outcome = escape_xml(outcome);
    let suite_seconds = format!("{:.3}", (duration_ms as f64) / 1000.0);
    let mut suite = format!(
        "<testsuite name=\"{escaped_suite}\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\" time=\"{suite_seconds}\">\n  <properties>\n    <property name=\"source\" value=\"deja-synthesized\"/>\n    <property name=\"mode\" value=\"batch\"/>\n    <property name=\"outcome\" value=\"{escaped_outcome}\"/>\n  </properties>\n",
        cases.len()
    );
    for tc in rendered_cases {
        suite.push_str(&tc);
        suite.push('\n');
    }
    suite.push_str("</testsuite>");

    merge_junit_suites(&[suite])
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Returns `true` when a JUnit XML document reports at least one failure, error, or non-`SUCCESS` status.
#[must_use]
pub fn reports_failure(xml: &str) -> bool {
    let bytes = xml.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<![CDATA[") {
            if let Some(end) = find_bytes(&bytes[i + 9..], b"]]>") {
                i += 9 + end + 3;
                continue;
            }
            break;
        }
        if bytes[i..].starts_with(b"<!--") {
            if let Some(end) = find_bytes(&bytes[i + 4..], b"-->") {
                i += 4 + end + 3;
                continue;
            }
            break;
        }
        let tag_end = find_tag_end(xml, i);
        let tag = &xml[i..tag_end];
        if bytes[i..tag_end].starts_with(b"<failure")
            && matches!(
                bytes.get(i + 8),
                Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')
            )
        {
            return true;
        }
        if bytes[i..tag_end].starts_with(b"<error")
            && matches!(
                bytes.get(i + 6),
                Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')
            )
        {
            return true;
        }
        if let Some(status) = extract_tag_attr(tag, "status")
            && status != "SUCCESS"
        {
            return true;
        }
        i = tag_end;
    }
    false
}

/// Finds the byte index immediately AFTER the closing `>` of an XML tag starting at `start`,
/// respecting single- and double-quoted attribute values containing `>`.
fn find_tag_end(xml: &str, start: usize) -> usize {
    let bytes = xml.as_bytes();
    let mut i = start + 1;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return i + 1,
                _ => {}
            },
        }
        i += 1;
    }
    bytes.len()
}

fn is_xml_name_delim(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '=' | '/' | '>' | '<' | '"' | '\'')
}

/// Redacts all non-structural attribute values inside a single XML tag `<...>`.
fn redact_tag_attributes<F: Fn(&str) -> String>(tag: &str, redact: &F) -> String {
    if tag.starts_with("<?") || tag.starts_with("<!") || tag.starts_with("</") {
        return tag.to_string();
    }

    let bytes = tag.as_bytes();
    let mut out = String::with_capacity(tag.len());
    let mut i = 0;

    // Copy `<tagName` verbatim.
    if i < bytes.len() && bytes[i] == b'<' {
        out.push('<');
        i += 1;
    }
    while let Some(ch) = tag[i..].chars().next() {
        if ch.is_whitespace() || matches!(ch, '>' | '/') {
            break;
        }
        out.push(ch);
        i += ch.len_utf8();
    }

    while i < bytes.len() {
        // Copy whitespace.
        let ws_start = i;
        while let Some(ch) = tag[i..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            i += ch.len_utf8();
        }
        out.push_str(&tag[ws_start..i]);
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'>' || bytes[i] == b'/' {
            out.push_str(&tag[i..]);
            break;
        }

        // Parse full XML attribute name (including multi-byte UTF-8 Unicode name characters).
        let name_start = i;
        while let Some(ch) = tag[i..].chars().next() {
            if is_xml_name_delim(ch) {
                break;
            }
            i += ch.len_utf8();
        }
        if i == name_start {
            // Unexpected delimiter character: copy verbatim and advance by one char.
            let ch = tag[i..].chars().next().expect("valid UTF-8");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let name = &tag[name_start..i];
        let mut cursor = i;
        while let Some(ch) = tag[cursor..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            cursor += ch.len_utf8();
        }
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            out.push_str(&tag[name_start..cursor]);
            i = cursor;
            continue;
        }
        cursor += 1; // skip '='
        while let Some(ch) = tag[cursor..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            cursor += ch.len_utf8();
        }
        if cursor >= bytes.len() || !matches!(bytes[cursor], b'"' | b'\'') {
            out.push_str(&tag[name_start..cursor]);
            i = cursor;
            continue;
        }
        let quote = bytes[cursor];
        let val_start = cursor + 1;
        let mut val_end = val_start;
        while val_end < bytes.len() && bytes[val_end] != quote {
            val_end += 1;
        }
        if val_end >= bytes.len() {
            // Unterminated attribute (e.g. truncated XML): redact non-structural remainder
            // rather than leaking raw bytes verbatim.
            if STRUCTURAL_ATTRIBUTES.contains(&name) {
                out.push_str(&tag[name_start..]);
            } else {
                out.push_str(&tag[name_start..cursor]);
                out.push(quote as char);
                let decoded = decode_xml(&tag[val_start..]);
                let redacted = redact(&decoded);
                out.push_str(&escape_xml(&redacted));
            }
            break;
        }
        let raw_val = &tag[val_start..val_end];
        let attr_end = val_end + 1;

        if STRUCTURAL_ATTRIBUTES.contains(&name) {
            out.push_str(&tag[name_start..attr_end]);
        } else {
            out.push_str(&tag[name_start..cursor]);
            out.push('"');
            let decoded = decode_xml(raw_val);
            let redacted = redact(&decoded);
            out.push_str(&escape_xml(&redacted));
            out.push('"');
        }
        i = attr_end;
    }

    out
}

/// Runs `redact` across every human-readable XML region (non-structural attribute values,
/// text nodes decoded before redaction and re-escaped after, CDATA with `]]>` split protection,
/// and comments), preserving structural tags, processing instructions, and counts/status attributes.
#[must_use]
pub fn redact_junit<F>(xml: &str, redact: F) -> String
where
    F: Fn(&str) -> String,
{
    let bytes = xml.as_bytes();
    let mut out = String::with_capacity(xml.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"<![CDATA[") {
            let content_start = i + 9;
            if let Some(rel_end) = find_bytes(&bytes[content_start..], b"]]>") {
                let inner = &xml[content_start..content_start + rel_end];
                let redacted = redact(inner);
                out.push_str("<![CDATA[");
                out.push_str(&escape_cdata(&redacted));
                out.push_str("]]>");
                i = content_start + rel_end + 3;
            } else {
                let inner = &xml[content_start..];
                let redacted = redact(inner);
                out.push_str("<![CDATA[");
                out.push_str(&escape_cdata(&redacted));
                out.push_str("]]>");
                break;
            }
            continue;
        }

        if bytes[i..].starts_with(b"<!--") {
            let content_start = i + 4;
            if let Some(rel_end) = find_bytes(&bytes[content_start..], b"-->") {
                let inner = &xml[content_start..content_start + rel_end];
                let redacted = redact(inner);
                out.push_str("<!--");
                out.push_str(&sanitize_comment(&redacted));
                out.push_str("-->");
                i = content_start + rel_end + 3;
            } else {
                let inner = &xml[content_start..];
                let redacted = redact(inner);
                out.push_str("<!--");
                out.push_str(&sanitize_comment(&redacted));
                out.push_str("-->");
                break;
            }
            continue;
        }

        if bytes[i] == b'<' {
            let tag_end = find_tag_end(xml, i);
            let tag = &xml[i..tag_end];
            out.push_str(&redact_tag_attributes(tag, &redact));
            i = tag_end;
            continue;
        }

        // Text node up to next '<'.
        let text_start = i;
        while i < bytes.len() && bytes[i] != b'<' {
            i += 1;
        }
        let segment = &xml[text_start..i];
        let decoded = decode_xml(segment);
        let redacted = redact(&decoded);
        out.push_str(&escape_xml(&redacted));
    }

    out
}

/// Convenience helper chaining `deja_core::Redactor` + `deja_core::ProviderKeyRedactor`
/// over a JUnit XML document via [`redact_junit`].
#[must_use]
pub fn redact_junit_with_redactors(
    xml: &str,
    redactor: &Redactor,
    provider: &ProviderKeyRedactor,
) -> String {
    redact_junit(xml, |text| provider.redact(&redactor.redact(text)))
}

fn extract_tag_attr<'a>(tag: &'a str, target_name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut i = 1; // skip '<'
    while let Some(ch) = tag[i..].chars().next() {
        if ch.is_whitespace() || matches!(ch, '>' | '/') {
            break;
        }
        i += ch.len_utf8();
    }
    while i < bytes.len() {
        while let Some(ch) = tag[i..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            i += ch.len_utf8();
        }
        if i >= bytes.len() || bytes[i] == b'>' || bytes[i] == b'/' {
            break;
        }
        let name_start = i;
        while let Some(ch) = tag[i..].chars().next() {
            if is_xml_name_delim(ch) {
                break;
            }
            i += ch.len_utf8();
        }
        if i == name_start {
            let ch = tag[i..].chars().next().expect("valid UTF-8");
            i += ch.len_utf8();
            continue;
        }
        let name = &tag[name_start..i];
        while let Some(ch) = tag[i..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            i += ch.len_utf8();
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while let Some(ch) = tag[i..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            i += ch.len_utf8();
        }
        if i >= bytes.len() || !matches!(bytes[i], b'"' | b'\'') {
            continue;
        }
        let quote = bytes[i];
        let val_start = i + 1;
        let mut val_end = val_start;
        while val_end < bytes.len() && bytes[val_end] != quote {
            val_end += 1;
        }
        if val_end >= bytes.len() {
            break;
        }
        if name == target_name {
            return Some(&tag[val_start..val_end]);
        }
        i = val_end + 1;
    }
    None
}

fn extract_testsuite_blocks(xml: &str) -> Vec<&str> {
    let mut suites = Vec::new();
    let bytes = xml.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<![CDATA[") {
            if let Some(end) = find_bytes(&bytes[i + 9..], b"]]>") {
                i += 9 + end + 3;
                continue;
            }
            break;
        }
        if bytes[i..].starts_with(b"<!--") {
            if let Some(end) = find_bytes(&bytes[i + 4..], b"-->") {
                i += 4 + end + 3;
                continue;
            }
            break;
        }
        if bytes[i..].starts_with(b"<testsuite")
            && matches!(
                bytes.get(i + 10),
                Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')
            )
        {
            let suite_start = i;
            let open_end = find_tag_end(xml, i);
            let open_tag = &xml[suite_start..open_end];
            if open_tag.ends_with("/>") {
                suites.push(open_tag);
                i = open_end;
                continue;
            }
            // Scan forward to matching `</testsuite>`, tracking nested `<testsuite>` if any
            // while skipping CDATA, comments, and quoted tag attributes on byte slices.
            let mut depth = 1usize;
            let mut cursor = open_end;
            while cursor < bytes.len() && depth > 0 {
                if bytes[cursor] != b'<' {
                    cursor += 1;
                    continue;
                }
                if bytes[cursor..].starts_with(b"<![CDATA[") {
                    if let Some(end) = find_bytes(&bytes[cursor + 9..], b"]]>") {
                        cursor += 9 + end + 3;
                        continue;
                    }
                    cursor = bytes.len();
                    break;
                }
                if bytes[cursor..].starts_with(b"<!--") {
                    if let Some(end) = find_bytes(&bytes[cursor + 4..], b"-->") {
                        cursor += 4 + end + 3;
                        continue;
                    }
                    cursor = bytes.len();
                    break;
                }
                let end = find_tag_end(xml, cursor);
                let tag_bytes = &bytes[cursor..end];
                if tag_bytes.starts_with(b"<testsuite")
                    && matches!(
                        tag_bytes.get(10),
                        Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')
                    )
                    && !tag_bytes.ends_with(b"/>")
                {
                    depth += 1;
                } else if tag_bytes.starts_with(b"</testsuite")
                    && matches!(tag_bytes.get(11), Some(b' ' | b'\t' | b'\r' | b'\n' | b'>'))
                {
                    depth -= 1;
                    if depth == 0 {
                        suites.push(&xml[suite_start..end]);
                        cursor = end;
                        break;
                    }
                }
                cursor = end;
            }
            i = cursor;
            continue;
        }
        i = find_tag_end(xml, i);
    }

    suites
}

/// Merges multiple JUnit XML reports into a single `<testsuites>` document, aggregating
/// top-level `tests`, `failures`, `errors`, `skipped`, and `time` totals.
#[must_use]
pub fn merge_junit_suites<S: AsRef<str>>(reports: &[S]) -> String {
    let mut blocks: Vec<&str> = Vec::new();
    let mut total_tests: u64 = 0;
    let mut total_failures: u64 = 0;
    let mut total_errors: u64 = 0;
    let mut total_skipped: u64 = 0;
    let mut total_time: f64 = 0.0;

    for report in reports {
        for suite in extract_testsuite_blocks(report.as_ref()) {
            let open_end = find_tag_end(suite, 0);
            let open_tag = &suite[..open_end];
            if let Some(v) = extract_tag_attr(open_tag, "tests").and_then(|s| s.parse::<u64>().ok())
            {
                total_tests = total_tests.saturating_add(v);
            }
            if let Some(v) =
                extract_tag_attr(open_tag, "failures").and_then(|s| s.parse::<u64>().ok())
            {
                total_failures = total_failures.saturating_add(v);
            }
            if let Some(v) =
                extract_tag_attr(open_tag, "errors").and_then(|s| s.parse::<u64>().ok())
            {
                total_errors = total_errors.saturating_add(v);
            }
            if let Some(v) =
                extract_tag_attr(open_tag, "skipped").and_then(|s| s.parse::<u64>().ok())
            {
                total_skipped = total_skipped.saturating_add(v);
            }
            if let Some(v) = extract_tag_attr(open_tag, "time").and_then(|s| s.parse::<f64>().ok())
                && v.is_finite()
                && v >= 0.0
            {
                total_time += v;
            }
            blocks.push(suite);
        }
    }

    let time_str = format!("{total_time:.3}");
    if blocks.is_empty() {
        return format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites tests=\"0\" failures=\"0\" errors=\"0\" skipped=\"0\" time=\"{time_str}\"/>\n"
        );
    }

    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites tests=\"{total_tests}\" failures=\"{total_failures}\" errors=\"{total_errors}\" skipped=\"{total_skipped}\" time=\"{time_str}\">\n"
    );
    for block in blocks {
        for line in block.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("</testsuites>\n");
    out
}

/// Removes any stale JUnit report at `path` and ensures its parent directory exists.
pub fn prepare_junit(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Atomically writes `contents` to `path` via a unique sibling temporary file + rename,
/// removing the temporary file if any error occurs.
pub fn atomic_write_junit(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "junit.xml".to_string());
    let temp_path = path.with_file_name(format!(".{file_name}.{pid}.{seq}.tmp"));

    let write_res = fs::write(&temp_path, contents).and_then(|()| fs::rename(&temp_path, path));
    if write_res.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_res
}

/// Atomically writes a synthesized failure JUnit fallback when a run aborts before a normal report.
pub fn write_errored_junit(path: &Path, goal: &str, message: &str) -> io::Result<()> {
    let xml = synthesized_junit(&SynthesizedJunitInput {
        goal,
        passed: false,
        reason: message,
        duration_ms: 0,
    });
    atomic_write_junit(path, &xml)
}

/// Redacts `xml` with both `redactor` and `provider`, then writes atomically to `path`.
pub fn write_redacted_junit(
    path: &Path,
    xml: &str,
    redactor: &Redactor,
    provider: &ProviderKeyRedactor,
) -> io::Result<()> {
    let redacted = redact_junit_with_redactors(xml, redactor, provider);
    atomic_write_junit(path, &redacted)
}
