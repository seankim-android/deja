//! Tests for `secret_scan` and `redaction` (`create_redactor`, `redact_json`, `secret_floor`,
//! `is_key_like_name`, `provider_key_redactor`, and 1 MB hostile string latency guarantees).

use std::time::Instant;

use deja_core::{
    CoreError, KEY_FLOOR, MIN_FORBIDDEN_VALUE_LENGTH, PASSWORD_FLOOR, bytes_hold_forbidden_value,
    create_redactor, holds_forbidden_value, is_key_like_name, json_holds_forbidden_value,
    provider_key_redactor, redact_json, secret_floor, spellings, validate_forbidden_values,
};
use serde_json::json;

// =============================================================================================
// secret_scan

#[test]
fn secret_scan_enforces_min_length_and_detects_all_encoded_spellings() {
    assert_eq!(MIN_FORBIDDEN_VALUE_LENGTH, 3);
    assert!(matches!(
        validate_forbidden_values(&["ab".into()]),
        Err(CoreError::ForbiddenValueTooShort { min_length: 3 })
    ));
    assert!(validate_forbidden_values(&["abc".into()]).is_ok());

    let secret = "p'ss/w\"rd@1";
    let forms = spellings(secret);
    assert!(forms.contains(&secret.to_string()));
    assert!(forms.contains(&"p''ss/w\"rd@1".to_string()));
    assert!(forms.iter().any(|f| f.contains("\\/")));
    assert!(forms.iter().any(|f| f.contains("%40")));

    let vals = vec![secret.to_string()];
    // Raw plain form, URL-encoded form, base64 form, and YAML decoded block/escaped scalar forms.
    assert!(holds_forbidden_value(
        &format!("- inputText: \"{secret}\""),
        &vals
    ));
    assert!(holds_forbidden_value(
        "url: https://x/?p=p'ss%2Fw%22rd%401",
        &vals
    ));
    assert!(holds_forbidden_value(
        "- inputText: |\n    p'ss/w\"rd@1\n",
        &vals
    ));
    assert!(holds_forbidden_value(
        "- inputText: 'p''ss/w\"rd@1'\n",
        &vals
    ));
    assert!(!holds_forbidden_value("- tapOn: safe\n", &vals));

    // Binary media scan catches raw and base64 spellings without YAML parsing.
    let mut blob = vec![0x89, 0x50, 0x4e, 0x47, 0x00, 0xff];
    blob.extend_from_slice(forms.last().unwrap().as_bytes());
    assert!(bytes_hold_forbidden_value(&blob, &vals));
    assert!(!bytes_hold_forbidden_value(
        &[0x89, 0x50, 0x4e, 0x47],
        &vals
    ));

    // JSON scan catches escaped and decoded strings (including keys).
    assert!(json_holds_forbidden_value(
        &json!({ "note": secret }).to_string(),
        &vals
    ));
    assert!(json_holds_forbidden_value(
        &format!(r#"{{"escaped":"{}"}}"#, "p'ss\\/w\\\"rd@1"),
        &vals
    ));
    assert!(!json_holds_forbidden_value(r#"{"safe":"value"}"#, &vals));

    // Deeply-nested (>200 levels) JSON with escaped credential still caught (no serde recursion limit bypass).
    let deep_secret = format!(
        "{}\"{}\"{}",
        "[".repeat(250),
        r"\u0073ecret-fixture",
        "]".repeat(250)
    );
    assert!(json_holds_forbidden_value(
        &deep_secret,
        &["secret-fixture".into()]
    ));
    let deep_safe = format!("{}\"innocent\"{}", "[".repeat(250), "]".repeat(250));
    assert!(!json_holds_forbidden_value(
        &deep_safe,
        &["secret-fixture".into()]
    ));
}

// =============================================================================================
// create_redactor & redact_json

#[test]
fn redactor_and_redact_json_hide_paths_fixtures_and_secret_keys() {
    let redactor = create_redactor(
        "/work/repo",
        Some("/home/tester"),
        &["hunter2-fixture".into()],
    );
    assert_eq!(
        redactor.redact("file /work/repo/a.yaml home /home/tester val hunter2-fixture"),
        "file <workspace>/a.yaml home <home> val <redacted-fixture-value>"
    );

    let input = json!({
        "goal": "login with hunter2-fixture",
        "path": "/work/repo/flow.yaml",
        "password": "plain-password",
        "nested": {
            "apiSecret": "top-secret",
            "authToken": "tok-value",
            "credential": "cred-value",
            "safe": "/home/tester/file",
            "count": 42,
        },
        "items": ["/work/repo/x"]
    });
    let redacted = redact_json(&input, &redactor);
    assert_eq!(redacted["goal"], "login with <redacted-fixture-value>");
    assert_eq!(redacted["path"], "<workspace>/flow.yaml");
    assert_eq!(redacted["password"], "<redacted>");
    assert_eq!(redacted["nested"]["apiSecret"], "<redacted>");
    assert_eq!(redacted["nested"]["authToken"], "<redacted>");
    assert_eq!(redacted["nested"]["credential"], "<redacted>");
    assert_eq!(redacted["nested"]["safe"], "<home>/file");
    assert_eq!(redacted["nested"]["count"], 42);
    assert_eq!(redacted["items"][0], "<workspace>/x");
}

// =============================================================================================
// secret_floor, is_key_like_name, provider_key_redactor (100% of apps/cli/test/mcp.test.ts)

#[test]
fn key_like_names_and_floors_match_typescript() {
    assert_eq!(PASSWORD_FLOOR, 4);
    assert_eq!(KEY_FLOOR, 8);

    let key_like = [
        "API_KEY",
        "SSH_KEY",
        "MY_TOKEN",
        "APIKEY",
        "apiKey",
        "githubToken",
        "DB_PASS",
        "DB_PWD",
        "AUTH",
        "SSH_AUTH_SOCK",
        "SESSION",
        "COOKIE",
        "PASSWD",
        "AWS_CREDENTIALS",
        "GPG_PRIVATE",
        "client.secret",
        "x-api-key",
        "XDG_SESSION_TYPE",
        "PGPASSWORD",
        "APITOKEN",
        "ACCESSTOKEN",
        "OPENAIAPIKEY",
        "ANTHROPICAPIKEY",
        "SECRETKEY",
        "AWSSECRETKEY",
        "DBPASS",
        "SSHKEY",
        "TOKEN2",
        "NPM_TOKEN2",
        "KEY2",
        "GITHUB_PAT",
        "JWT",
        "PGPASS",
        "SK",
        "STRIPE_SK",
        "stripeSk",
        "AWS_ACCESSKEY",
        "MYPRIVATEKEY",
    ];
    for name in key_like {
        assert!(is_key_like_name(name), "expected {name} to be key-like");
    }

    let ordinary = [
        "KEYBOARD",
        "MONKEY",
        "KEYS_DIR",
        "SESSIONNAME",
        "PATH",
        "HOME",
        "PWD",
        "OLDPWD",
        "PASSTHROUGH",
        "AUTHOR",
        "PASSAGE",
        "COMPASS",
        "PATCH",
        "PATTERN",
        "SSH_AGENT_PID",
        "USER",
        "SHELL",
    ];
    for name in ordinary {
        assert!(
            !is_key_like_name(name),
            "expected {name} NOT to be key-like"
        );
    }

    assert_eq!(secret_floor("DB_PASSWORD"), Some(4));
    assert_eq!(secret_floor("PGPASSWORD"), Some(4));
    assert_eq!(secret_floor("DB_PASS"), Some(4));
    assert_eq!(secret_floor("MY_PWD"), Some(4));
    assert_eq!(secret_floor("API_TOKEN"), Some(8));
    assert_eq!(secret_floor("API_KEY"), Some(8));
    assert_eq!(secret_floor("PWD"), None);

    let redact = provider_key_redactor([
        ("DB_PASSWORD", "hunter2"),
        ("PGPASSWORD", "qwer"),
        ("DB_PASS", "xyz"),
        ("MY_PWD", "pw12"),
        ("API_TOKEN", "abcdefg"),
        ("API_KEY", "abcdefgh"),
    ]);
    assert_eq!(
        redact.redact("hunter2 qwer xyz pw12 abcdefg abcdefgh"),
        "<redacted-secret> <redacted-secret> xyz <redacted-secret> abcdefg <redacted-secret>"
    );
}

#[test]
fn provider_shapes_and_url_passwords_redacted_while_ordinary_words_preserved() {
    let redact = provider_key_redactor([
        ("KEYBOARD", "dvorak-layout"),
        ("API_TOKEN", "tok3n-Zq81xv07"),
        ("SESSION_SECRET", &"x".repeat(30)),
        ("SHORT_KEY", "abc"),
    ]);

    // Build all token shapes dynamically at runtime so no literal keys sit in source.
    let sk_key = format!("sk-{}", "a1b2c3d4".repeat(4));
    let bearer = format!("Bearer {}", "Ab1.".repeat(8));

    let ordinary_line = format!(
        "Bearer authentication scheme with dvorak-layout and flow-{sk_key} and disk-{} task_{} risk_{}",
        "x".repeat(20),
        "x".repeat(20),
        "y".repeat(20)
    );
    assert_eq!(redact.redact(&ordinary_line), ordinary_line);

    assert_eq!(
        redact.redact(&format!(
            "key {sk_key}, header {bearer}, env tok3n-Zq81xv07 and {}",
            "x".repeat(30)
        )),
        "key <redacted-secret>, header Bearer <redacted-secret>, env <redacted-secret> and <redacted-secret>"
    );

    let empty_redact = provider_key_redactor(std::iter::empty::<(&str, &str)>());
    let samples = [
        format!("{}_{}", "ghp", "a1B2c3D4e5".repeat(4)),
        format!("{}_{}", "gho", "a1B2c3D4e5".repeat(4)),
        format!("{}_{}", "ghs", "a1B2c3D4e5".repeat(4)),
        format!("{}_{}", "ghu", "a1B2c3D4e5".repeat(4)),
        format!("{}_{}", "ghr", "a1B2c3D4e5".repeat(4)),
        format!(
            "{}_{}_{}",
            "github_pat",
            "A1b2C3d4E5".repeat(6),
            "z9".repeat(10)
        ),
        format!("{}{}", "AKIA", "Q7W8E9R2T3Y4U5I6"),
        format!("{}{}", "ASIA", "Q7W8E9R2T3Y4U5I6"),
        format!("{}-{}-{}", "xoxb", "1234567890", "abcdefghij"),
        format!("{}-{}-{}", "xoxp", "1234567890", "abcdefghij"),
        format!("{}-{}-x", "xoxa", "1234567890"),
        format!("{}-{}-x", "xoxr", "1234567890"),
        format!("{}-{}-x", "xoxe", "1234567890"),
        format!("{}-{}-x", "xoxs", "1234567890"),
    ];
    for sample in samples {
        assert_eq!(
            empty_redact.redact(&format!("before {sample} after")),
            "before <redacted-secret> after",
            "expected sample shape to redact"
        );
    }

    let safe_samples = [
        "ghost_story_title_of_the_week".to_string(),
        format!("{}_short", "ghp"),
        "AKIAshort".to_string(),
        format!("M{}{}", "AKIA", "Q7W8E9R2T3Y4U5I6"),
        format!("{}{}X", "AKIA", "Q7W8E9R2T3Y4U5I6"),
        "xoxz-1234567890-abcdefghij".to_string(),
        "xoxe-123".to_string(),
        format!("task_{} ask_{}", "x".repeat(20), "x".repeat(20)),
    ];
    for text in safe_samples {
        assert_eq!(empty_redact.redact(&text), text);
    }

    // Underscore provider keys.
    let live = format!("sk_live_{}", "a1b2c3d4".repeat(3));
    let lower = format!("sk_test_{}", "abcdefgh".repeat(3));
    let pk = format!("pk_live_{}", "q".repeat(20));
    assert_eq!(
        empty_redact.redact(&format!("{live} {lower} ({pk})")),
        "<redacted-secret> <redacted-secret> (<redacted-secret>)"
    );

    // Bearer ASCII word boundary and ECMAScript whitespace: non-ASCII letter preceding `Bearer`
    // and U+FEFF/U+00A0/U+2028 between `Bearer` and the token still redact, while ASCII word chars
    // before `Bearer` or U+0085 (not ES whitespace) do not.
    let bearer_tok = "A".repeat(24);
    assert_eq!(
        empty_redact.redact(&format!("éBearer {bearer_tok}")),
        "éBearer <redacted-secret>"
    );
    assert_eq!(
        empty_redact.redact(&format!("xBearer {bearer_tok}")),
        format!("xBearer {bearer_tok}")
    );
    assert_eq!(
        empty_redact.redact(&format!("Bearer\u{FEFF}{bearer_tok}")),
        "Bearer <redacted-secret>"
    );
    let bearer_sk = format!("Bearer\u{FEFF}{}_{}", "sk_test", "1234567890abcdef");
    assert_eq!(
        empty_redact.redact(&bearer_sk),
        "Bearer\u{FEFF}<redacted-secret>"
    );
    assert_eq!(
        empty_redact.redact(&format!("Bearer\u{0085}{bearer_tok}")),
        format!("Bearer\u{0085}{bearer_tok}")
    );

    // URL passwords.
    assert_eq!(
        empty_redact.redact("postgres://app:s3cr3t-pass@db.internal:5432/app"),
        "postgres://app:<redacted-secret>@db.internal:5432/app"
    );
    assert_eq!(
        empty_redact
            .redact("REDIS_URL=redis://:x@cache:6379 and rediss://default:abc123@cache:6380/0"),
        "REDIS_URL=redis://:<redacted-secret>@cache:6379 and rediss://default:<redacted-secret>@cache:6380/0"
    );
    assert_eq!(
        empty_redact.redact("redis://:secretpw@host:6379"),
        "redis://:<redacted-secret>@host:6379"
    );
    assert_eq!(
        empty_redact.redact("postgres://user:p@ss@host/db"),
        "postgres://user:<redacted-secret>@host/db"
    );
    assert_eq!(
        empty_redact.redact("postgres://user:p%40ss@host:5432/db?x=a@b"),
        "postgres://user:<redacted-secret>@host:5432/db?x=a@b"
    );
    let safe_urls = "https://example.com/path?x=a:b@c and http://user@host/ and https://host:8080/";
    assert_eq!(empty_redact.redact(safe_urls), safe_urls);
}

#[test]
fn redactor_stays_linear_on_1mb_hostile_strings() {
    let redact = provider_key_redactor([
        ("API_TOKEN", "abcdefghijklmnopqrstuv"),
        ("PGPASSWORD", "qwer"),
    ]);
    let megabyte = 1_000_000_usize;
    let repeat = |unit: &str| unit.repeat(megabyte.div_ceil(unit.len()));

    let inputs: Vec<(&str, String)> = vec![
        ("a", repeat("a")),
        ("a.", repeat("a.")),
        ("a-", repeat("a-")),
        ("a+", repeat("a+")),
        ("digits", repeat("1")),
        ("a:", repeat("a:")),
        ("a://", repeat("a://")),
        ("a://:", repeat("a://:")),
        ("a://: x1000", repeat(&format!("a://:{}", "x".repeat(1000)))),
        ("a://u:", repeat("a://u:")),
        ("a@", repeat("a@")),
        ("a:b@", repeat("a:b@")),
        ("scheme user run", format!("a://{}", "u".repeat(megabyte))),
        ("password run", format!("a://u:{}", "p".repeat(megabyte))),
        ("sk", format!("{}-{}", "sk", "a".repeat(megabyte))),
        ("sk_", repeat("sk_")),
        ("sk-", repeat("sk-")),
        ("pk", repeat("pk_a")),
        ("ghp", format!("{}_{}", "ghp", "a".repeat(megabyte))),
        ("ghp_ repeated", repeat("ghp_")),
        (
            "github_pat",
            format!("{}_{}", "github_pat", "a".repeat(megabyte)),
        ),
        ("github_pat_ repeated", repeat("github_pat_")),
        ("AKIA", format!("{}{}", "AKIA", "A".repeat(megabyte))),
        ("AKIA repeated", repeat("AKIA")),
        ("xox", format!("{}-{}", "xoxb", "a".repeat(megabyte))),
        ("xoxb- repeated", repeat("xoxb-")),
        ("Bearer", format!("Bearer {}", "a".repeat(megabyte))),
        ("Bearer repeated", repeat("Bearer ")),
        ("Bearer spaces", format!("Bearer{}", " ".repeat(megabyte))),
        ("secret value", repeat("abcdefghijklmnopqrstu")),
    ];

    let max_ms: u128 = if cfg!(debug_assertions) { 2_000 } else { 200 };
    let mut slow = Vec::new();
    for (name, input) in inputs {
        let started = Instant::now();
        let _ = redact.redact(&input);
        let elapsed_ms = started.elapsed().as_millis();
        // Unoptimized debug builds run `regex-automata` with debug assertions on every byte;
        // quadratic regexes take tens of seconds even in release builds.
        if elapsed_ms >= max_ms {
            slow.push((name, elapsed_ms));
        }
    }
    assert!(slow.is_empty(), "slow hostile redaction cases: {slow:?}");
}
