//! Pure-Rust git-style path glob compiler and iterative matcher (`DEJA-80`).
//!
//! Supports `*` (non-`/`), `?` (single non-`/`), `**` (collapsed consecutive `**`: `**/foo`
//! matches 0+ leading dirs, `a/**/b` matches 0+ middle dirs, `a/**` and `**` match 1+ trailing
//! segments), `[abc]`/`[a-z]`/`[!a-z]`/`[^a-z]` (never matching `/`), and `{a,b}` brace
//! expansion with strict nesting depth (`8`) and expansion variant (`256`) caps.

use crate::commands::manifest::has_unsafe_control_or_bidi_char;

/// Maximum `{...}` brace nesting depth permitted in a glob pattern.
pub const MAX_BRACE_DEPTH: usize = 8;

/// Maximum number of expanded brace variants permitted in a glob pattern.
pub const MAX_BRACE_VARIANTS: usize = 256;

/// Compiled pure-Rust git-style path glob pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledGlob {
    pattern: String,
    variants: Vec<CompiledVariant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledVariant {
    segments: Vec<SegmentPattern>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SegmentPattern {
    GlobStar,
    Matcher(Vec<Token>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(char),
    AnyChar,
    AnySeq,
    CharClass {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassItem {
    Single(char),
    Range(char, char),
}

impl ClassItem {
    #[inline]
    fn matches(self, ch: char) -> bool {
        match self {
            Self::Single(c) => ch == c,
            Self::Range(lo, hi) => ch >= lo && ch <= hi,
        }
    }
}

impl CompiledGlob {
    /// Original unexpanded glob pattern string.
    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Match a relative POSIX path against this compiled glob using iterative DP / two-pointer
    /// matching (`O(P * S)` worst-case, zero recursion).
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let mut normalized = path;
        while let Some(rest) = normalized.strip_prefix("./") {
            normalized = rest;
        }
        if normalized.is_empty() || normalized.starts_with('/') {
            return false;
        }
        let path_segs: Vec<&str> = normalized.split('/').collect();
        if path_segs.is_empty() {
            return false;
        }
        self.variants
            .iter()
            .any(|variant| match_segments(&variant.segments, &path_segs))
    }
}

/// Compile and validate a git-style relative path glob pattern.
pub fn compile_glob(pattern: &str) -> Result<CompiledGlob, String> {
    validate_raw_glob_hygiene(pattern)?;
    let expanded = expand_braces(pattern)?;
    let mut variants = Vec::with_capacity(expanded.len());
    for raw_variant in expanded {
        variants.push(compile_variant(&raw_variant)?);
    }
    Ok(CompiledGlob {
        pattern: pattern.to_string(),
        variants,
    })
}

fn has_windows_drive_prefix(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn validate_raw_glob_hygiene(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("glob pattern must not be empty or whitespace".to_string());
    }
    if pattern.contains('\0') || has_unsafe_control_or_bidi_char(pattern) {
        return Err(
            "glob pattern must not contain NUL, control, line-break, bidi, or zero-width characters"
                .to_string(),
        );
    }
    if pattern.contains('\\') {
        return Err("glob pattern must use `/` separators and not contain `\\`".to_string());
    }
    if pattern.starts_with('/') {
        return Err("glob pattern must be relative and must not start with `/`".to_string());
    }
    if has_windows_drive_prefix(pattern) {
        return Err("glob pattern must not contain a Windows drive prefix".to_string());
    }
    if pattern.split('/').any(|seg| seg == "..") {
        return Err("glob pattern must not contain `..` path segments".to_string());
    }
    Ok(())
}

fn expand_braces(pattern: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut pos = 0usize;
    let expanded = parse_brace_seq(&chars, &mut pos, false, 0)?;
    if pos != chars.len() {
        return Err("unexpected trailing characters in glob pattern".to_string());
    }
    Ok(expanded)
}

fn parse_brace_seq(
    chars: &[char],
    pos: &mut usize,
    stop_at_brace_delim: bool,
    depth: usize,
) -> Result<Vec<String>, String> {
    let mut acc: Vec<String> = vec![String::new()];

    while *pos < chars.len() {
        let ch = chars[*pos];
        if stop_at_brace_delim && matches!(ch, ',' | '}') {
            break;
        }
        if !stop_at_brace_delim && ch == '}' {
            return Err("unmatched `}` in glob pattern".to_string());
        }
        if ch == '[' {
            let class_text = scan_char_class_raw(chars, pos)?;
            for item in &mut acc {
                item.push_str(&class_text);
            }
            continue;
        }
        if ch == '{' {
            let next_depth = depth + 1;
            if next_depth > MAX_BRACE_DEPTH {
                return Err(format!(
                    "glob brace nesting depth exceeds limit of {MAX_BRACE_DEPTH}"
                ));
            }
            *pos += 1; // consume `{`
            let mut alts: Vec<String> = Vec::new();
            let mut saw_comma = false;
            loop {
                let branch = parse_brace_seq(chars, pos, true, next_depth)?;
                if alts.len().saturating_add(branch.len()) > MAX_BRACE_VARIANTS {
                    return Err(format!(
                        "glob brace expansion exceeds limit of {MAX_BRACE_VARIANTS} variants"
                    ));
                }
                alts.extend(branch);
                if *pos >= chars.len() {
                    return Err("unclosed `{` in glob pattern".to_string());
                }
                match chars[*pos] {
                    ',' => {
                        saw_comma = true;
                        *pos += 1;
                    }
                    '}' => {
                        *pos += 1;
                        break;
                    }
                    _ => return Err("malformed brace expression in glob pattern".to_string()),
                }
            }
            if !saw_comma && alts.len() == 1 && alts[0].is_empty() {
                return Err("empty `{}` brace expression in glob pattern".to_string());
            }
            let total = acc.len().saturating_mul(alts.len());
            if total > MAX_BRACE_VARIANTS {
                return Err(format!(
                    "glob brace expansion exceeds limit of {MAX_BRACE_VARIANTS} variants"
                ));
            }
            let mut combined = Vec::with_capacity(total);
            for prefix in &acc {
                for alt in &alts {
                    let mut s = String::with_capacity(prefix.len() + alt.len());
                    s.push_str(prefix);
                    s.push_str(alt);
                    combined.push(s);
                }
            }
            acc = combined;
            continue;
        }

        for item in &mut acc {
            item.push(ch);
        }
        *pos += 1;
    }

    Ok(acc)
}

fn scan_char_class_raw(chars: &[char], pos: &mut usize) -> Result<String, String> {
    let mut buf = String::new();
    buf.push(chars[*pos]); // '['
    *pos += 1;

    if *pos < chars.len() && matches!(chars[*pos], '!' | '^') {
        buf.push(chars[*pos]);
        *pos += 1;
    }

    while *pos < chars.len() {
        let ch = chars[*pos];
        if ch == '/' {
            return Err("character class `[...]` in glob must not contain `/`".to_string());
        }
        buf.push(ch);
        *pos += 1;
        if ch == ']' {
            return Ok(buf);
        }
    }
    Err("unclosed `[` character class in glob pattern".to_string())
}

fn compile_variant(raw_variant: &str) -> Result<CompiledVariant, String> {
    let mut s = raw_variant;
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    if s.trim().is_empty() {
        return Err("expanded glob variant must not be empty".to_string());
    }
    if s.starts_with('/') {
        return Err("expanded glob variant must not start with `/`".to_string());
    }
    if has_windows_drive_prefix(s) {
        return Err("expanded glob variant must not contain a Windows drive prefix".to_string());
    }
    if s.split('/').any(|seg| seg == "..") {
        return Err("expanded glob variant must not contain `..` path segments".to_string());
    }

    let owned_expanded: String;
    let normalized = if s.ends_with('/') {
        let trimmed = s.trim_end_matches('/');
        if trimmed.is_empty() {
            return Err("expanded glob variant must not be empty".to_string());
        }
        owned_expanded = format!("{trimmed}/**");
        owned_expanded.as_str()
    } else {
        s
    };

    let mut segments: Vec<SegmentPattern> = Vec::new();
    for seg in normalized.split('/') {
        if seg == "**" {
            // Collapse consecutive `**` segments into a single `GlobStar`.
            if !matches!(segments.last(), Some(SegmentPattern::GlobStar)) {
                segments.push(SegmentPattern::GlobStar);
            }
        } else {
            let tokens = compile_segment_tokens(seg)?;
            segments.push(SegmentPattern::Matcher(tokens));
        }
    }

    if segments.is_empty() {
        return Err("glob pattern produced no path segments".to_string());
    }

    Ok(CompiledVariant { segments })
}

fn compile_segment_tokens(seg: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = seg.chars().collect();
    let mut tokens: Vec<Token> = Vec::with_capacity(chars.len());
    let mut i = 0usize;

    while i < chars.len() {
        match chars[i] {
            '*' => {
                // Collapse consecutive `*` inside one segment.
                if !matches!(tokens.last(), Some(Token::AnySeq)) {
                    tokens.push(Token::AnySeq);
                }
                i += 1;
            }
            '?' => {
                tokens.push(Token::AnyChar);
                i += 1;
            }
            '[' => {
                i += 1;
                let negated = if i < chars.len() && matches!(chars[i], '!' | '^') {
                    i += 1;
                    true
                } else {
                    false
                };
                let mut items = Vec::new();
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == ']' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    let lo = chars[i];
                    if lo == '/' {
                        return Err(
                            "character class `[...]` in glob must not contain `/`".to_string()
                        );
                    }
                    if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
                        let hi = chars[i + 2];
                        if hi == '/' {
                            return Err(
                                "character class `[...]` in glob must not contain `/`".to_string()
                            );
                        }
                        if lo > hi {
                            return Err(format!(
                                "inverted character class range `{lo}-{hi}` in glob pattern"
                            ));
                        }
                        items.push(ClassItem::Range(lo, hi));
                        i += 3;
                    } else {
                        items.push(ClassItem::Single(lo));
                        i += 1;
                    }
                }
                if !closed {
                    return Err("unclosed `[` character class in glob pattern".to_string());
                }
                if items.is_empty() {
                    return Err("empty character class `[]` in glob pattern".to_string());
                }
                tokens.push(Token::CharClass { negated, items });
            }
            ch => {
                tokens.push(Token::Literal(ch));
                i += 1;
            }
        }
    }

    Ok(tokens)
}

/// Iterative DP segment matcher (`O(P * S)` worst-case, zero recursion).
///
/// Semantics of `**` (`SegmentPattern::GlobStar`):
/// - Non-trailing `**` (`**/foo`, `a/**/b`): matches **0 or more** path segments.
/// - Trailing `**` (`a/**`, `**`): matches **1 or more** path segments.
fn match_segments(pat_segs: &[SegmentPattern], path_segs: &[&str]) -> bool {
    let m = pat_segs.len();
    let n = path_segs.len();
    let mut dp = vec![false; n + 1];
    dp[0] = true;

    for (i, pat) in pat_segs.iter().enumerate() {
        let mut next = vec![false; n + 1];
        match pat {
            SegmentPattern::GlobStar => {
                let is_trailing = i + 1 == m;
                if is_trailing {
                    // Trailing `**` requires at least 1 segment (`next[j]` true if any `dp[k]` for `k < j`).
                    let mut seen_prev = false;
                    for j in 1..=n {
                        if dp[j - 1] {
                            seen_prev = true;
                        }
                        next[j] = seen_prev;
                    }
                } else {
                    // Leading/middle `**` matches 0 or more segments (`next[j]` true if any `dp[k]` for `k <= j`).
                    let mut seen = false;
                    for j in 0..=n {
                        if dp[j] {
                            seen = true;
                        }
                        next[j] = seen;
                    }
                }
            }
            SegmentPattern::Matcher(tokens) => {
                for j in 0..n {
                    if dp[j] && match_segment_tokens(tokens, path_segs[j]) {
                        next[j + 1] = true;
                    }
                }
            }
        }
        dp = next;
    }

    dp[n]
}

/// Iterative two-pointer wildcard matcher within a single path segment (no `/`).
fn match_segment_tokens(tokens: &[Token], seg: &str) -> bool {
    let chars: Vec<char> = seg.chars().collect();
    let mut ti = 0usize;
    let mut ci = 0usize;
    let mut star_ti: Option<usize> = None;
    let mut star_ci = 0usize;

    while ci < chars.len() {
        if ti < tokens.len() && token_matches_char(&tokens[ti], chars[ci]) {
            ti += 1;
            ci += 1;
        } else if ti < tokens.len() && matches!(tokens[ti], Token::AnySeq) {
            star_ti = Some(ti);
            star_ci = ci;
            ti += 1;
        } else if let Some(st) = star_ti {
            star_ci += 1;
            ci = star_ci;
            ti = st + 1;
        } else {
            return false;
        }
    }

    while ti < tokens.len() && matches!(tokens[ti], Token::AnySeq) {
        ti += 1;
    }
    ti == tokens.len()
}

#[inline]
fn token_matches_char(token: &Token, ch: char) -> bool {
    if ch == '/' {
        return false;
    }
    match token {
        Token::Literal(c) => ch == *c,
        Token::AnyChar => true,
        Token::AnySeq => false,
        Token::CharClass { negated, items } => {
            let hit = items.iter().any(|item| item.matches(ch));
            if *negated { !hit } else { hit }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_stars_question_marks_and_dot_slash_normalization() {
        let g = compile_glob("flows/*.yaml").unwrap();
        assert!(g.matches("flows/login.yaml"));
        assert!(g.matches("./flows/login.yaml"));
        assert!(!g.matches("flows/sub/login.yaml"));
        assert!(!g.matches("flows/login.yml"));

        let g2 = compile_glob("./flows/step-?.yaml").unwrap();
        assert!(g2.matches("flows/step-1.yaml"));
        assert!(g2.matches("flows/step-a.yaml"));
        assert!(!g2.matches("flows/step-10.yaml"));
        assert!(!g2.matches("flows/step-/.yaml"));

        // Trailing `/` expands to `/**` (1+ segments under directory).
        let dir = compile_glob("src/checkout/").unwrap();
        assert!(dir.matches("src/checkout/cart.swift"));
        assert!(dir.matches("src/checkout/deep/nested.swift"));
        assert!(!dir.matches("src/checkout"));
        assert!(!dir.matches("src/checkout2/cart.swift"));
    }

    #[test]
    fn globstar_leading_middle_trailing_and_collapsed() {
        // Leading `**/foo`: 0+ leading dirs.
        let lead = compile_glob("**/foo.yaml").unwrap();
        assert!(lead.matches("foo.yaml"));
        assert!(lead.matches("a/foo.yaml"));
        assert!(lead.matches("a/b/c/foo.yaml"));
        assert!(!lead.matches("barfoo.yaml"));
        assert!(!lead.matches("a/foo.yaml/bar"));

        // Middle `a/**/b`: 0+ middle dirs.
        let mid = compile_glob("a/**/b.yaml").unwrap();
        assert!(mid.matches("a/b.yaml"));
        assert!(mid.matches("a/x/b.yaml"));
        assert!(mid.matches("a/x/y/z/b.yaml"));
        assert!(!mid.matches("b.yaml"));
        assert!(!mid.matches("a/b.yaml/c"));

        // Trailing `a/**`: 1+ trailing segments (must NOT match bare `a`).
        let trail = compile_glob("a/**").unwrap();
        assert!(trail.matches("a/x"));
        assert!(trail.matches("a/x/y"));
        assert!(!trail.matches("a"));
        assert!(!trail.matches("ab/x"));

        // Bare `**`: 1+ segments (never empty string).
        let all = compile_glob("**").unwrap();
        assert!(all.matches("README.md"));
        assert!(all.matches("a/b/c"));
        assert!(!all.matches(""));

        // Consecutive `**/**` collapses cleanly.
        let collapsed = compile_glob("a/**/**/**/b").unwrap();
        assert!(collapsed.matches("a/b"));
        assert!(collapsed.matches("a/m/n/b"));
    }

    #[test]
    fn character_classes_negation_ranges_and_slash_exclusion() {
        let g = compile_glob("flows/test-[a-c0-2].yaml").unwrap();
        assert!(g.matches("flows/test-a.yaml"));
        assert!(g.matches("flows/test-c.yaml"));
        assert!(g.matches("flows/test-1.yaml"));
        assert!(!g.matches("flows/test-d.yaml"));
        assert!(!g.matches("flows/test-3.yaml"));

        let neg1 = compile_glob("flows/[!0-9].yaml").unwrap();
        assert!(neg1.matches("flows/a.yaml"));
        assert!(!neg1.matches("flows/5.yaml"));

        let neg2 = compile_glob("flows/[^a-z].yaml").unwrap();
        assert!(neg2.matches("flows/9.yaml"));
        assert!(!neg2.matches("flows/m.yaml"));

        // Range spanning `/` (`.` is 46, `/` is 47, `0` is 48) must never match `/`.
        let spanning = compile_glob("a[.-0]b").unwrap();
        assert!(spanning.matches("a.b"));
        assert!(spanning.matches("a0b"));
        assert!(!spanning.matches("a/b"));

        // Leading/trailing hyphen inside class is literal `-`.
        let dash = compile_glob("a[-z]b").unwrap();
        assert!(dash.matches("a-b"));
        assert!(dash.matches("azb"));
    }

    #[test]
    fn brace_expansion_nested_and_inside_char_classes() {
        let g = compile_glob("src/{ios,android}/{auth,billing}/**/*.{swift,kt}").unwrap();
        assert!(g.matches("src/ios/auth/Login.swift"));
        assert!(g.matches("src/android/billing/deep/Pay.kt"));
        assert!(!g.matches("src/web/auth/Login.swift"));

        // Non-segment `**` (`**.kt`) behaves like `*` within a single segment (does not cross `/`).
        let intra = compile_glob("src/**.kt").unwrap();
        assert!(intra.matches("src/Pay.kt"));
        assert!(!intra.matches("src/deep/Pay.kt"));

        // Trailing `/` inside brace variant expands to `/**`.
        let dirs = compile_glob("apps/{ios/,android/}").unwrap();
        assert!(dirs.matches("apps/ios/Main.swift"));
        assert!(dirs.matches("apps/android/Main.kt"));
        assert!(!dirs.matches("apps/ios"));

        // Nested braces up to depth 8 succeed.
        let depth8 = compile_glob("{{{{{{{{a,b}}}}}}}}").unwrap();
        assert!(depth8.matches("a"));
        assert!(depth8.matches("b"));
        assert!(!depth8.matches("c"));

        // `{`, `}`, `,` inside `[...]` are literal class members, NOT brace syntax.
        let in_class = compile_glob("file[{,}]x").unwrap();
        assert!(in_class.matches("file{x"));
        assert!(in_class.matches("file,x"));
        assert!(in_class.matches("file}x"));
        assert!(!in_class.matches("fileax"));

        let mixed = compile_glob("{[a,b],c}.yaml").unwrap();
        assert!(mixed.matches("a.yaml"));
        assert!(mixed.matches(",.yaml"));
        assert!(mixed.matches("b.yaml"));
        assert!(mixed.matches("c.yaml"));
    }

    #[test]
    fn rejects_malformed_unsafe_and_dos_patterns() {
        let bad_cases = [
            "",
            "   ",
            "foo\0bar",
            "foo\nbar",
            "foo\u{202E}bar",
            r"flows\login.yaml",
            "/etc/passwd",
            "C:/Windows/System32",
            "c:foo.yaml",
            "../secret.yaml",
            "flows/../secret.yaml",
            "./",
            ".//foo.yaml",
            "flows/[abc",
            "flows/[]",
            "flows/[!]",
            "flows/[^]",
            "flows/[z-a].yaml",
            "flows/[a/b].yaml",
            "flows/{a,b",
            "flows/a,b}",
            "flows/{}.yaml",
            "flows/{..,ok}/a.yaml",
            "{/abs,rel}/a.yaml",
            "{C:,rel}/a.yaml",
            // Depth 9 > MAX_BRACE_DEPTH (8)
            "{{{{{{{{{a,b}}}}}}}}}",
            // 2^9 = 512 > MAX_BRACE_VARIANTS (256)
            "{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}",
        ];
        for pat in bad_cases {
            assert!(
                compile_glob(pat).is_err(),
                "expected pattern {pat:?} to be rejected"
            );
        }
    }

    #[test]
    fn pathological_star_pattern_finishes_in_polynomial_time() {
        let g = compile_glob("a*a*a*a*a*a*a*a*a*a*b").unwrap();
        let long_non_match = "a".repeat(200);
        assert!(!g.matches(&long_non_match));
        let long_match = format!("{}b", "a".repeat(200));
        assert!(g.matches(&long_match));
    }
}
