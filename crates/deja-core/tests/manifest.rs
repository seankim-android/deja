//! Tests for walk manifest v1 parsing, deterministic serialization, recursive duplicate JSON key
//! rejection, `DEJA-38` N4 path spelling collision checks, and pure domain contracts/gate evaluation.

use std::collections::BTreeMap;

use deja_core::{
    ArtifactKind, CONTRACT_VERSION, CacheMissReason, CacheStatus, CoreError,
    DEFAULT_FLOW_SCHEMA_VERSION, FailureCategory, FlowChange, GateFlakyFailure, GateIdentity,
    GateOptions, GateOutcome, LaunchArgumentValue, LookupHit, LookupMiss, LookupResult,
    MANIFEST_HASH_VERSION, ManifestOwnerRules, ManifestProfile, ManifestWalk, RunArtifact,
    RunCommand, RunOutcome, RunResult, StoredValidation, StoredWalk, WALK_MANIFEST_VERSION,
    WALK_RESULT_SCHEMA_VERSION, WalkFailure, WalkIdentity, WalkManifest, WalkOrigin,
    WalkValidation, check_manifest_flow_path, empty_walk_manifest, evaluate_gate_from_records,
    gate_passed, manifest_walk_key, normalize_goal, parse_walk_manifest, serialize_walk_manifest,
};
use serde_json::{Number, json};

fn sha(ch: char) -> String {
    std::iter::repeat_n(ch, 64).collect()
}

fn sample_walk() -> ManifestWalk {
    ManifestWalk {
        goal: "Today".into(),
        app_id: "com.example.app".into(),
        platform: "ios".into(),
        adapter: "maestro-ios/v1".into(),
        browser: None,
        flow: ".maestro/flows/today.yaml".into(),
        closure_sha256: sha('a'),
        assertions_sha256: sha('b'),
        owner_rules: None,
        profile: None,
    }
}

fn manifest_json(walks: serde_json::Value) -> String {
    json!({
        "manifestVersion": 1,
        "hashVersion": 1,
        "walks": walks,
    })
    .to_string()
}

// =============================================================================================
// packages/core/test/manifest.test.ts (all 5 test blocks) + DEJA-38 N4 & DEJA-57 fields

#[test]
fn round_trips_through_fixed_serialization_sorted_with_no_timestamps() {
    let mut launch_args = BTreeMap::new();
    launch_args.insert("uiTesting".to_string(), LaunchArgumentValue::Bool(true));
    launch_args.insert(
        "count".to_string(),
        LaunchArgumentValue::Number(Number::from(2)),
    );
    launch_args.insert(
        "mode".to_string(),
        LaunchArgumentValue::String("fast".to_string()),
    );

    let smoke = ManifestWalk {
        goal: "Smoke".into(),
        flow: ".maestro/flows/smoke.yaml".into(),
        owner_rules: Some(ManifestOwnerRules {
            min_assertions: Some(2),
            required_assertion_ids: Some(vec!["screen.today".into()]),
            disallow_text_selectors: Some(true),
            allowed_text_selectors: Some(vec!["Allow Once".into()]),
        }),
        profile: Some(ManifestProfile {
            name: Some("uiTesting".into()),
            launch_arguments: Some(launch_args),
            clear_state: Some(true),
        }),
        ..sample_walk()
    };

    let manifest = WalkManifest {
        walks: vec![sample_walk(), smoke],
        ..empty_walk_manifest()
    };
    let text = serialize_walk_manifest(&manifest).expect("serialize");
    assert!(text.ends_with("}\n"));
    assert!(text.find("\"Smoke\"").unwrap() < text.find("\"Today\"").unwrap());
    assert!(text.contains(&format!("\"manifestVersion\": {WALK_MANIFEST_VERSION}")));
    assert!(text.contains(&format!("\"hashVersion\": {MANIFEST_HASH_VERSION}")));
    // Verify launchArguments sorted alphabetically (`count`, `mode`, `uiTesting`) and integer `2` kept intact.
    assert!(text.find("\"count\": 2").unwrap() < text.find("\"mode\": \"fast\"").unwrap());
    assert!(text.find("\"mode\": \"fast\"").unwrap() < text.find("\"uiTesting\": true").unwrap());

    let parsed = parse_walk_manifest(&text).expect("parse");
    assert_eq!(
        parsed
            .walks
            .iter()
            .map(|w| w.goal.as_str())
            .collect::<Vec<_>>(),
        vec!["Smoke", "Today"]
    );
    assert_eq!(serialize_walk_manifest(&parsed).expect("reserialize"), text);
}

#[test]
fn fails_closed_on_unknown_version_unknown_field_or_non_object() {
    let base_entry = serde_json::to_value(sample_walk()).unwrap();
    let mut extra_walk = base_entry.clone();
    extra_walk["extra"] = json!(1);
    let mut bad_profile = base_entry.clone();
    bad_profile["profile"] = json!({ "name": "x", "secret": "y" });
    let mut bad_rules = base_entry.clone();
    bad_rules["ownerRules"] = json!({ "other": 1 });

    for text in [
        "".to_string(),
        "[]".to_string(),
        json!({ "manifestVersion": 2, "hashVersion": 1, "walks": [] }).to_string(),
        json!({ "hashVersion": 1, "walks": [] }).to_string(),
        json!({ "manifestVersion": 1, "hashVersion": 2, "walks": [] }).to_string(),
        json!({ "manifestVersion": 1, "hashVersion": 1, "walks": [], "extra": 1 }).to_string(),
        manifest_json(json!([extra_walk])),
        manifest_json(json!([bad_profile])),
        manifest_json(json!([bad_rules])),
    ] {
        assert!(
            matches!(
                parse_walk_manifest(&text),
                Err(CoreError::InvalidManifest(_))
            ),
            "expected InvalidManifest for {text}"
        );
    }
}

#[test]
fn refuses_duplicate_json_keys_at_every_depth() {
    let good = serialize_walk_manifest(&WalkManifest {
        walks: vec![sample_walk()],
        ..empty_walk_manifest()
    })
    .expect("serialize");

    // Duplicate inside walk object.
    let twice = good.replace(
        "\"closureSha256\"",
        &format!(
            "\"assertionsSha256\": \"{}\",\n      \"closureSha256\"",
            sha('c')
        ),
    );
    assert_ne!(twice, good);
    assert!(
        parse_walk_manifest(&twice)
            .unwrap_err()
            .to_string()
            .contains("appears twice")
    );

    // Duplicate at top level.
    assert!(
        parse_walk_manifest(
            r#"{"manifestVersion":1,"manifestVersion":1,"hashVersion":1,"walks":[]}"#
        )
        .unwrap_err()
        .to_string()
        .contains("appears twice")
    );

    // Duplicate deeply nested inside profile.launchArguments.
    let deep_dup = format!(
        r#"{{"manifestVersion":1,"hashVersion":1,"walks":[{{"goal":"Today","appId":"com.example.app","platform":"ios","adapter":"maestro-ios/v1","flow":".maestro/flows/today.yaml","closureSha256":"{}","assertionsSha256":"{}","profile":{{"launchArguments":{{"mode":"a","mode":"b"}}}}}}]}}"#,
        sha('a'),
        sha('b')
    );
    assert!(
        parse_walk_manifest(&deep_dup)
            .unwrap_err()
            .to_string()
            .contains("appears twice")
    );

    // Equal keys in different objects and braces inside strings are allowed.
    let valid = serialize_walk_manifest(&WalkManifest {
        walks: vec![
            ManifestWalk {
                goal: "a {\"x\": 1, \"x\": 2} b".into(),
                ..sample_walk()
            },
            ManifestWalk {
                goal: "Other".into(),
                ..sample_walk()
            },
        ],
        ..empty_walk_manifest()
    })
    .expect("serialize");
    assert_eq!(parse_walk_manifest(&valid).expect("parse").walks.len(), 2);
}

#[test]
fn refuses_unsafe_flow_paths() {
    for flow in [
        "/abs/today.yaml",
        "../today.yaml",
        ".maestro/../../today.yaml",
        ".maestro\\flows\\today.yaml",
        "C:/today.yaml",
        ".deja/flows/today.yaml",
        ".DEJA/flows/today.yaml",
        ".Deja/x.yaml",
        ".maestro//today.yaml",
        "./today.yaml",
        "today.json",
        "",
        "flows/line\nbreak.yaml",
    ] {
        assert!(
            matches!(
                check_manifest_flow_path(flow, "flow"),
                Err(CoreError::InvalidManifest(_))
            ),
            "expected InvalidManifest for flow path {flow:?}"
        );
    }
}

#[test]
fn refuses_machine_data_control_chars_bad_hashes_duplicates_and_deja38_n4_collisions() {
    let base = serde_json::to_value(sample_walk()).unwrap();
    let patch = |f: fn(&mut serde_json::Value)| {
        let mut v = base.clone();
        f(&mut v);
        manifest_json(json!([v]))
    };

    let bad_manifests = [
        patch(|v| v["goal"] = json!("/Users/someone/work")),
        patch(|v| v["appId"] = json!("~/app")),
        patch(|v| v["profile"] = json!({ "name": "C:\\Users\\x" })),
        patch(|v| v["profile"] = json!({ "launchArguments": { "apiKey": "x" } })),
        patch(|v| v["profile"] = json!({ "launchArguments": { "path": "/home/someone/a" } })),
        patch(|v| v["profile"] = json!({ "launchArguments": { "nested": { "a": 1 } } })),
        patch(|v| v["closureSha256"] = json!("ABC")),
        patch(|v| v["assertionsSha256"] = json!(&sha('b')[1..])),
        patch(|v| v["platform"] = json!("tv")),
        patch(|v| v["platform"] = json!("web")),
        patch(|v| v["browser"] = json!("chromium")),
        patch(|v| v["goal"] = json!("   ")),
        patch(|v| v["goal"] = json!("line1\nline2")),
        patch(|v| v["ownerRules"] = json!({ "disallowTextSelectors": "yes" })),
        patch(|v| v["ownerRules"] = json!({ "allowedTextSelectors": [123] })),
    ];
    for text in bad_manifests {
        assert!(
            matches!(
                parse_walk_manifest(&text),
                Err(CoreError::InvalidManifest(_))
            ),
            "expected rejection for {text}"
        );
    }

    // Duplicate goal key fails closed.
    let dup_goal = manifest_json(json!([
        sample_walk(),
        ManifestWalk {
            goal: " TODAY ".into(),
            ..sample_walk()
        }
    ]));
    assert!(
        parse_walk_manifest(&dup_goal)
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );

    // Identical literal flow path across platforms is allowed.
    let cross_platform = manifest_json(json!([
        sample_walk(),
        ManifestWalk {
            platform: "android".into(),
            adapter: "maestro-android".into(),
            ..sample_walk()
        }
    ]));
    assert_eq!(
        parse_walk_manifest(&cross_platform)
            .expect("valid")
            .walks
            .len(),
        2
    );

    // Web walk with browser succeeds.
    let web_walk = manifest_json(json!([ManifestWalk {
        platform: "web".into(),
        adapter: "maestro-web@2".into(),
        browser: Some("chromium".into()),
        app_id: "http://127.0.0.1:4173".into(),
        ..sample_walk()
    }]));
    assert_eq!(
        parse_walk_manifest(&web_walk).expect("valid").walks[0]
            .browser
            .as_deref(),
        Some("chromium")
    );

    // DEJA-38 N4: case-only flow spelling collision rejected.
    let case_collision = manifest_json(json!([
        sample_walk(),
        ManifestWalk {
            goal: "Other".into(),
            flow: ".maestro/flows/TODAY.yaml".into(),
            ..sample_walk()
        }
    ]));
    assert!(
        parse_walk_manifest(&case_collision)
            .unwrap_err()
            .to_string()
            .contains("case or Unicode normalization")
    );

    // DEJA-38 N4: NFC vs NFD flow spelling collision rejected.
    let nfc_collision = manifest_json(json!([
        ManifestWalk {
            goal: "Cafe A".into(),
            flow: ".maestro/flows/caf\u{00e9}.yaml".into(),
            ..sample_walk()
        },
        ManifestWalk {
            goal: "Cafe B".into(),
            flow: ".maestro/flows/cafe\u{0301}.yaml".into(),
            ..sample_walk()
        }
    ]));
    assert!(
        parse_walk_manifest(&nfc_collision)
            .unwrap_err()
            .to_string()
            .contains("case or Unicode normalization")
    );

    let key = manifest_walk_key(&sample_walk()).expect("walk key");
    assert_eq!(
        key,
        r#"["today","com.example.app","ios","maestro-ios/v1",null]"#
    );
}

#[test]
fn manifest_accepts_integral_floats_and_matches_js_launch_arg_key_and_number_formatting() {
    let raw = format!(
        r#"{{"manifestVersion":1.0,"hashVersion":1e0,"walks":[{{"goal":"T","appId":"a","platform":"ios","adapter":"x","flow":"a.yaml","closureSha256":"{}","assertionsSha256":"{}","ownerRules":{{"minAssertions":2.0}},"profile":{{"launchArguments":{{"10":1.0,"2":2,"b":3,"a":4,"01":5,"negZero":-0.0}}}}}}]}}"#,
        sha('a'),
        sha('a')
    );
    let parsed = parse_walk_manifest(&raw).expect("integral floats accepted");
    assert_eq!(
        parsed.walks[0].owner_rules.as_ref().unwrap().min_assertions,
        Some(2)
    );

    let serialized = serialize_walk_manifest(&parsed).expect("serialize");
    // Exact JS Object enumeration order for launchArguments: array indices ("2", "10") numerically
    // first, then UTF-16 sorted non-index keys ("01", "a", "b", "negZero"), and 1.0 -> 1, -0.0 -> 0.
    let expected_block = concat!(
        "        \"launchArguments\": {\n",
        "          \"2\": 2,\n",
        "          \"10\": 1,\n",
        "          \"01\": 5,\n",
        "          \"a\": 4,\n",
        "          \"b\": 3,\n",
        "          \"negZero\": 0\n",
        "        }",
    );
    assert!(
        serialized.contains(expected_block),
        "unexpected launchArguments block:\n{serialized}"
    );

    // Non-integral floats and negative integral floats still fail closed.
    let frac_min = format!(
        r#"{{"manifestVersion":1,"hashVersion":1,"walks":[{{"goal":"T","appId":"a","platform":"ios","adapter":"x","flow":"a.yaml","closureSha256":"{}","assertionsSha256":"{}","ownerRules":{{"minAssertions":2.5}}}}]}}"#,
        sha('a'),
        sha('a')
    );
    assert!(matches!(
        parse_walk_manifest(&frac_min),
        Err(CoreError::InvalidManifest(_))
    ));
    let neg_min = format!(
        r#"{{"manifestVersion":1,"hashVersion":1,"walks":[{{"goal":"T","appId":"a","platform":"ios","adapter":"x","flow":"a.yaml","closureSha256":"{}","assertionsSha256":"{}","ownerRules":{{"minAssertions":-1.0}}}}]}}"#,
        sha('a'),
        sha('a')
    );
    assert!(matches!(
        parse_walk_manifest(&neg_min),
        Err(CoreError::InvalidManifest(_))
    ));
}

// =============================================================================================
// Contracts & Gate evaluation tests

#[test]
fn contracts_normalize_goal_serde_and_gate_evaluation() {
    assert_eq!(WALK_RESULT_SCHEMA_VERSION, 1);
    assert_eq!(CONTRACT_VERSION, 1);
    assert_eq!(DEFAULT_FLOW_SCHEMA_VERSION, "maestro/v1");

    let g = normalize_goal("  CHECKOUT\t\n now  ").expect("valid goal");
    assert_eq!(g.text, "CHECKOUT\t\n now");
    assert_eq!(g.key, "checkout now");
    assert!(matches!(
        normalize_goal("   \t\n"),
        Err(CoreError::EmptyGoal)
    ));

    // Exact ECMAScript whitespace: U+0085 NEXT LINE is NOT whitespace, while U+00A0/U+2028/U+3000/FEFF are.
    let nel = normalize_goal("a\u{0085}b").expect("nel goal");
    assert_eq!(nel.text, "a\u{0085}b");
    assert_eq!(nel.key, "a\u{0085}b");
    let es_ws =
        normalize_goal("\u{FEFF}\u{00A0}Hello\u{2028}\u{3000}World\u{00A0}").expect("es ws");
    assert_eq!(es_ws.text, "Hello\u{2028}\u{3000}World");
    assert_eq!(es_ws.key, "hello world");

    let run = RunResult {
        schema_version: WALK_RESULT_SCHEMA_VERSION,
        command: RunCommand::Run,
        identity: WalkIdentity {
            goal: "Checkout".into(),
            app_id: "com.example.app".into(),
            platform: "ios".into(),
            flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.into(),
            build_fingerprint: sha('1'),
            adapter: Some("maestro-ios/v1".into()),
            browser: None,
        },
        goal_key: "checkout".into(),
        cache_status: CacheStatus::Hit,
        cache_miss_reason: None,
        semantic: None,
        outcome: RunOutcome::ProductRegression,
        passed: false,
        started_at: "2026-11-26T00:00:00.000Z".into(),
        ended_at: "2026-11-26T00:00:01.000Z".into(),
        duration_ms: 1000,
        model_calls: Some(0),
        model_call_basis: "zero-model-replay".into(),
        assertions: Some(2),
        repair_count: 0,
        yaml_path: Some(".maestro/flows/checkout.yaml".into()),
        yaml_sha256: Some(sha('2')),
        failure: Some(WalkFailure {
            category: FailureCategory::ProductAssertion,
            advisory: true,
            repair_eligible: false,
            failing_step: Some("assertVisible".into()),
            raw_artifact_path: Some(".deja/reports/out.log".into()),
            crash: None,
        }),
        artifacts: vec![RunArtifact {
            kind: ArtifactKind::Output,
            path: ".deja/reports/out.log".into(),
            sha256: None,
        }],
        diffs: vec![],
        errors: vec![],
        execution: None,
        exploration: None,
        repair: None,
        replayed_on_new_build: None,
        engine: None,
        attempts: None,
    };
    let wire = serde_json::to_value(&run).expect("serialize RunResult");
    assert_eq!(wire["schemaVersion"], 1);
    assert_eq!(wire["outcome"], "product-regression");
    assert_eq!(wire["failure"]["category"], "product_assertion");
    assert!(wire.get("cacheMissReason").is_none());
    let roundtrip: RunResult = serde_json::from_value(wire).expect("deserialize RunResult");
    assert_eq!(roundtrip, run);

    // Gate evaluation across all outcomes + flaky attribution + deduplication.
    let build = sha('9');
    let identity = GateIdentity {
        app_id: "com.example.app".into(),
        platform: "ios".into(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.into(),
        build_fingerprint: build.clone(),
        adapter: Some("maestro-ios/v1".into()),
        browser: None,
    };
    let passing_walk = StoredWalk {
        goal: "Today".into(),
        app_id: identity.app_id.clone(),
        platform: identity.platform.clone(),
        flow_schema_version: identity.flow_schema_version.clone(),
        build_fingerprint: build.clone(),
        adapter: identity.adapter.clone(),
        browser: None,
        goal_key: "today".into(),
        flow_path: ".maestro/flows/today.yaml".into(),
        flow_sha256: sha('a'),
        subflow_files: None,
        legacy_closure: None,
        closure_sha256: Some(sha('c')),
        last_validation: WalkValidation {
            passed: true,
            at: "2026-11-26T00:00:00.000Z".into(),
        },
        validation_history: vec![StoredValidation {
            passed: true,
            at: "2026-11-26T00:00:00.000Z".into(),
            flow_sha256: sha('a'),
        }],
        origin: Some(WalkOrigin::Remember),
        exploration: None,
        repair_history: None,
    };
    let flaky_walk = StoredWalk {
        goal: "Flaky".into(),
        goal_key: "flaky".into(),
        last_validation: WalkValidation {
            passed: false,
            at: "2026-11-26T00:01:00.000Z".into(),
        },
        ..passing_walk.clone()
    };
    let walks = vec![passing_walk.clone(), flaky_walk.clone()];
    let options = GateOptions {
        flaky_failures: vec![GateFlakyFailure {
            goal_key: "flaky".into(),
            build_fingerprint: build.clone(),
            at: "2026-11-26T00:01:00.000Z".into(),
        }],
    };
    let goals = vec![
        "Today".into(),
        " today ".into(), // deduplicated
        "Flaky".into(),
        "Changed".into(),
        "Missing".into(),
        "Broken".into(),
        "Unrun".into(),
    ];
    let gate_results =
        evaluate_gate_from_records(&walks, &goals, &identity, true, &options, |id| {
            match id.goal.as_str() {
                "Today" => Ok(LookupResult::Hit(Box::new(LookupHit {
                    walk: passing_walk.clone(),
                    flow_path: "/abs/today.yaml".into(),
                }))),
                "Flaky" => Ok(LookupResult::Miss(LookupMiss {
                    reason: CacheMissReason::FailedValidation,
                    flow_change: None,
                    closure_sha256: Some(sha('c')),
                    flow_path: Some("/abs/flaky.yaml".into()),
                })),
                "Changed" => Ok(LookupResult::Miss(LookupMiss {
                    reason: CacheMissReason::FlowChanged,
                    flow_change: Some(FlowChange {
                        manifest_closure_sha256: sha('1'),
                        current_closure_sha256: sha('2'),
                    }),
                    closure_sha256: None,
                    flow_path: None,
                })),
                "Missing" => Ok(LookupResult::Miss(LookupMiss {
                    reason: CacheMissReason::MissingFlow,
                    flow_change: None,
                    closure_sha256: None,
                    flow_path: None,
                })),
                "Broken" => Err("checksum mismatch".into()),
                _ => Ok(LookupResult::Miss(LookupMiss {
                    reason: CacheMissReason::NotValidated,
                    flow_change: None,
                    closure_sha256: None,
                    flow_path: None,
                })),
            }
        })
        .expect("gate eval");

    assert_eq!(gate_results.len(), 6);
    assert_eq!(gate_results[0].outcome, GateOutcome::Passed);
    assert_eq!(gate_results[1].outcome, GateOutcome::Flaky);
    assert_eq!(gate_results[2].outcome, GateOutcome::FlowChanged);
    assert_eq!(gate_results[3].outcome, GateOutcome::Invalid);
    assert_eq!(gate_results[4].outcome, GateOutcome::Invalid);
    assert_eq!(gate_results[5].outcome, GateOutcome::NotRun);
    assert!(!gate_passed(&gate_results));
    assert!(gate_passed(&gate_results[..1]));
    assert!(!gate_passed(&[]));
}
