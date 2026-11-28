//! Unit tests for manifest `ownerRules` (`minAssertions`, `requiredAssertionIds`,
//! and `DEJA-57` `disallowTextSelectors` / `allowedTextSelectors`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use deja_core::{
    CoreError, ManifestOwnerRules, TextSelectorViolation, check_owner_rules, closure_assertions,
    collect_flow_closure, find_disallowed_text_selectors,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "deja-owner-rules-{prefix}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        let canonical = fs::canonicalize(&dir).expect("canonicalize temp dir");
        Self { path: canonical }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, rel: &str, contents: &str) {
        let target = self.path.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&target, contents).expect("write fixture file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn min_assertions_and_required_assertion_ids_enforced_unconditionally() {
    let tmp = TempDir::new("min-and-req");
    tmp.write(
        "flows/main.yaml",
        concat!(
            "appId: com.example\n---\n",
            "- assertVisible:\n    id: \"screen.today\"\n",
            "- assertVisible:\n    id: \"optional.id\"\n    optional: true\n",
            "- runFlow:\n    file: sub.yaml\n    when:\n      platform: iOS\n",
        ),
    );
    tmp.write(
        "flows/sub.yaml",
        "- assertVisible:\n    id: \"conditional.sub\"\n",
    );

    let closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("flows/main.yaml")).expect("closure");
    let assertions = closure_assertions(&closure).expect("assertions");

    // 3 assertVisible commands total -> minAssertions: 3 passes, 4 fails.
    let ok_rules = ManifestOwnerRules {
        min_assertions: Some(3),
        required_assertion_ids: Some(vec!["screen.today".into()]),
        ..Default::default()
    };
    assert!(check_owner_rules(Some(&ok_rules), &assertions, Some(&closure)).is_ok());

    let too_few = ManifestOwnerRules {
        min_assertions: Some(4),
        ..Default::default()
    };
    assert!(matches!(
        check_owner_rules(Some(&too_few), &assertions, Some(&closure)),
        Err(CoreError::OwnerRule(_))
    ));

    // Optional and conditionally-called assertVisible do NOT satisfy requiredAssertionIds.
    for id in ["optional.id", "conditional.sub"] {
        let req = ManifestOwnerRules {
            required_assertion_ids: Some(vec![id.into()]),
            ..Default::default()
        };
        assert!(
            matches!(
                check_owner_rules(Some(&req), &assertions, Some(&closure)),
                Err(CoreError::OwnerRule(_))
            ),
            "expected {id} not to satisfy unconditional requiredAssertionIds"
        );
    }
}

#[test]
fn deja57_flags_bare_strings_and_text_only_maps_allows_id_anchored_and_allowlisted() {
    let tmp = TempDir::new("deja57-basic");
    tmp.write(
        "flows/main.yaml",
        concat!(
            "appId: com.example\n---\n",
            "- tapOn:\n    id: \"btn.continue\"\n",
            "- assertVisible:\n    id: \"answer.field\"\n    text: \"expected.*\"\n",
            "- inputText: \"Hello world\"\n",
            "- assertTrue: \"${output.ok}\"\n",
            "- tapOn: \"Allow While Using App\"\n",
            "- tapOn: \"Continue\"\n",
            "- assertVisible:\n    text: \"Order confirmed\"\n",
            "- assertNotVisible:\n    id: \"\"\n    text: \"Blank ID error\"\n",
            "- doubleTapOn: \"Zoom\"\n",
            "- longPressOn:\n    text: \"Hold me\"\n",
            "- copyTextFrom: \"Promo code\"\n",
        ),
    );
    let closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("flows/main.yaml")).expect("closure");
    let assertions = closure_assertions(&closure).expect("assertions");

    let violations =
        find_disallowed_text_selectors(&closure, &["Allow While Using App".into()]).expect("lint");
    assert_eq!(
        violations,
        vec![
            TextSelectorViolation {
                file: "main.yaml".into(),
                command: "tapOn".into(),
                selector: "Continue".into(),
            },
            TextSelectorViolation {
                file: "main.yaml".into(),
                command: "assertVisible".into(),
                selector: "Order confirmed".into(),
            },
            TextSelectorViolation {
                file: "main.yaml".into(),
                command: "assertNotVisible".into(),
                selector: "Blank ID error".into(),
            },
            TextSelectorViolation {
                file: "main.yaml".into(),
                command: "doubleTapOn".into(),
                selector: "Zoom".into(),
            },
            TextSelectorViolation {
                file: "main.yaml".into(),
                command: "longPressOn".into(),
                selector: "Hold me".into(),
            },
            TextSelectorViolation {
                file: "main.yaml".into(),
                command: "copyTextFrom".into(),
                selector: "Promo code".into(),
            },
        ]
    );

    let rules = ManifestOwnerRules {
        disallow_text_selectors: Some(true),
        allowed_text_selectors: Some(vec!["Allow While Using App".into()]),
        ..Default::default()
    };
    let err = check_owner_rules(Some(&rules), &assertions, Some(&closure)).unwrap_err();
    assert!(matches!(err, CoreError::OwnerRule(_)));
    assert!(err.to_string().contains("Continue"));
}

#[test]
fn deja57_inspects_compound_selectors_and_nested_subflows_without_flagging_non_selectors() {
    let tmp = TempDir::new("deja57-compound");
    tmp.write(
        "flows/main.yaml",
        concat!(
            "appId: com.example\n",
            "onFlowStart:\n",
            "  - assertVisible: \"Start banner\"\n",
            "---\n",
            "- extendedWaitUntil:\n",
            "    visible: \"Loaded\"\n",
            "    notVisible:\n",
            "      text: \"Spinner\"\n",
            "    timeout: 5000\n",
            "- scrollUntilVisible:\n",
            "    element:\n",
            "      text: \"Footer\"\n",
            "- repeat:\n",
            "    while:\n",
            "      visible: \"Next page\"\n",
            "    commands:\n",
            "      - inputText: \"typed text is fine\"\n",
            "      - assertTrue: \"${1 === 1}\"\n",
            "- runFlow:\n",
            "    when:\n",
            "      visible: \"Cookie banner\"\n",
            "    file: ../subflows/child.yaml\n",
            "- retry:\n",
            "    when:\n",
            "      notVisible:\n",
            "        text: \"Done\"\n",
            "    commands:\n",
            "      - swipe:\n",
            "          direction: LEFT\n",
            "      - swipe:\n",
            "          from: \"Carousel card\"\n",
            "          direction: LEFT\n",
        ),
    );
    tmp.write(
        "subflows/child.yaml",
        "- tapOn:\n    id: \"ok.button\"\n- assertVisible: \"Subflow text\"\n",
    );

    let closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("flows/main.yaml")).expect("closure");
    let violations = find_disallowed_text_selectors(&closure, &[]).expect("lint");

    assert_eq!(
        violations,
        vec![
            TextSelectorViolation {
                file: "flows/main.yaml".into(),
                command: "assertVisible".into(),
                selector: "Start banner".into(),
            },
            TextSelectorViolation {
                file: "flows/main.yaml".into(),
                command: "extendedWaitUntil".into(),
                selector: "Loaded".into(),
            },
            TextSelectorViolation {
                file: "flows/main.yaml".into(),
                command: "extendedWaitUntil".into(),
                selector: "Spinner".into(),
            },
            TextSelectorViolation {
                file: "flows/main.yaml".into(),
                command: "scrollUntilVisible".into(),
                selector: "Footer".into(),
            },
            TextSelectorViolation {
                file: "flows/main.yaml".into(),
                command: "repeat.while".into(),
                selector: "Next page".into(),
            },
            TextSelectorViolation {
                file: "flows/main.yaml".into(),
                command: "runFlow.when".into(),
                selector: "Cookie banner".into(),
            },
            TextSelectorViolation {
                file: "flows/main.yaml".into(),
                command: "retry.when".into(),
                selector: "Done".into(),
            },
            TextSelectorViolation {
                file: "flows/main.yaml".into(),
                command: "swipe.from".into(),
                selector: "Carousel card".into(),
            },
            TextSelectorViolation {
                file: "subflows/child.yaml".into(),
                command: "assertVisible".into(),
                selector: "Subflow text".into(),
            },
        ]
    );
}

#[test]
fn deja57_passes_when_all_selectors_use_ids_or_allowlist() {
    let tmp = TempDir::new("deja57-clean");
    tmp.write(
        "flows/main.yaml",
        concat!(
            "appId: com.example\n---\n",
            "- tapOn:\n    id: \"tab.today\"\n",
            "- inputText: \"My journal entry\"\n",
            "- assertVisible:\n    id: \"entry.body\"\n    text: \"My journal.*\"\n",
            "- extendedWaitUntil:\n    visible:\n      id: \"screen.saved\"\n    timeout: 5000\n",
            "- scrollUntilVisible:\n    element:\n      id: \"row.bottom\"\n",
            "- swipe:\n    from:\n      id: \"card.1\"\n    direction: LEFT\n",
            "- assertTrue: \"${output.saved}\"\n",
            "- tapOn: \"Allow\"\n",
        ),
    );
    let closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("flows/main.yaml")).expect("closure");
    let assertions = closure_assertions(&closure).expect("assertions");
    let rules = ManifestOwnerRules {
        disallow_text_selectors: Some(true),
        allowed_text_selectors: Some(vec!["Allow".into()]),
        ..Default::default()
    };
    assert!(check_owner_rules(Some(&rules), &assertions, Some(&closure)).is_ok());
}

#[test]
fn deja57_rejects_text_selectors_in_run_script_when_conditions() {
    let tmp = TempDir::new("deja57-run-script-when");
    tmp.write("flows/helper.js", "output.ok = true;\n");
    tmp.write(
        "flows/main.yaml",
        concat!(
            "appId: com.example\n---\n",
            "- runScript:\n",
            "    file: helper.js\n",
            "    when:\n",
            "      visible: Continue\n",
            "      notVisible: Loading\n",
            "- assertVisible:\n",
            "    id: done\n",
        ),
    );
    let closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("flows/main.yaml")).expect("closure");
    let assertions = closure_assertions(&closure).expect("assertions");
    let rules = ManifestOwnerRules {
        disallow_text_selectors: Some(true),
        ..Default::default()
    };
    let msg = check_owner_rules(Some(&rules), &assertions, Some(&closure))
        .expect_err("runScript.when text selectors must be rejected")
        .to_string();
    assert!(msg.contains("runScript.when"), "unexpected err: {msg}");
    assert!(msg.contains("Continue"), "unexpected err: {msg}");
    assert!(msg.contains("Loading"), "unexpected err: {msg}");
}
