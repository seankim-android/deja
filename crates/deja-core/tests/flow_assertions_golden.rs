//! Golden parity tests for YAML 1.2 parsing, `<<` merge expansion, ES262 canonical JSON,
//! `find_flow_assertions`, `closure_assertions`, and `assertions_sha256`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use deja_core::{
    ASSERTION_HASH_VERSION, FlowAssertionCallKind, FlowReferenceKind, MAX_ALIAS_EXPANSIONS,
    MAX_MATERIALIZED_BYTES, YamlValue, assertions_sha256, closure_assertions, collect_flow_closure,
    find_flow_assertions, js_json_number_to_string, js_number_to_string, parse_yaml_documents,
};
use sha2::{Digest, Sha256};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "deja-core-test-{prefix}-{}-{id}",
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

fn entries(yaml: &str) -> Vec<String> {
    find_flow_assertions(yaml)
        .expect("YAML should parse")
        .into_iter()
        .map(|a| a.entry)
        .collect()
}

fn single_file_assertions_hash(yaml: &str) -> String {
    let lines = entries(yaml);
    let payload = format!(
        "deja-assertions-v{ASSERTION_HASH_VERSION}\n{}\n",
        lines.join("\n")
    );
    hex::encode(Sha256::digest(payload.as_bytes()))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root should exist")
}

// =============================================================================================
// 100% of packages/core/test/flow-assertions.test.ts (all 10 test blocks)

#[test]
fn assertion_hash_version_is_1() {
    assert_eq!(ASSERTION_HASH_VERSION, 1);
}

#[test]
fn lists_whole_commands_with_sorted_keys_whatever_the_yaml_spelling() {
    let yaml = [
        "appId: com.example",
        "---",
        "- assertVisible:",
        "    id: \"screen.today\"",
        "- assertNotVisible:",
        "    text: \"Error.*\"",
        "    id: \"today.error\"",
        "- assertVisible: \"Order confirmed\"",
        "- assertTrue: ${output.ok}",
        "",
    ]
    .join("\n");
    assert_eq!(
        entries(&yaml),
        vec![
            "|assertVisible|{\"id\":\"screen.today\"}",
            "|assertNotVisible|{\"id\":\"today.error\",\"text\":\"Error.*\"}",
            "|assertVisible|\"Order confirmed\"",
            "|assertTrue|\"${output.ok}\"",
        ]
    );
    assert_eq!(
        entries("- assertNotVisible: {text: \"Error.*\", id: \"today.error\"}\n"),
        vec!["|assertNotVisible|{\"id\":\"today.error\",\"text\":\"Error.*\"}"]
    );
}

#[test]
fn any_extra_field_changes_the_entry() {
    let base = entries("- assertVisible:\n    id: \"a\"\n");
    assert_ne!(
        entries("- assertVisible:\n    id: \"a\"\n    optional: true\n"),
        base
    );
    assert_ne!(
        entries("- assertVisible:\n    id: \"a\"\n    enabled: true\n"),
        base
    );
}

#[test]
fn records_where_an_assertion_sits_and_parameters_of_calls() {
    let yaml = [
        "- repeat:",
        "    times: 2",
        "    commands:",
        "      - assertVisible:",
        "          id: \"row\"",
        "- runFlow:",
        "    when:",
        "      visible: \"Banner\"",
        "    commands:",
        "      - assertVisible: \"inline\"",
        "- runFlow: ../subflows/type.yaml",
        "- retry:",
        "    maxRetries: 3",
        "    file: ../subflows/flaky.yaml",
        "",
    ]
    .join("\n");
    assert_eq!(
        entries(&yaml),
        vec![
            "|repeat|params={\"times\":2}",
            "repeat{\"times\":2}>commands|assertVisible|{\"id\":\"row\"}",
            "|runFlow|params={\"when\":{\"visible\":\"Banner\"}}",
            "runFlow{\"when\":{\"visible\":\"Banner\"}}>commands|assertVisible|\"inline\"",
            "|runFlow|params={}",
            "|retry|params={\"maxRetries\":3}",
        ]
    );
}

#[test]
fn weaker_repeat_retry_condition_or_env_value_changes_entries() {
    let base = |times: &str| {
        entries(&format!(
            "- repeat:\n    times: {times}\n    commands:\n      - assertVisible: \"row\"\n"
        ))
    };
    assert_ne!(base("2"), base("0"));

    let looping = |cond: &str| {
        entries(&format!(
            "- repeat:\n    while:\n      visible: \"{cond}\"\n    commands:\n      - assertVisible: \"row\"\n"
        ))
    };
    assert_ne!(looping("A"), looping("B"));

    let flow = |env_val: &str| {
        entries(&format!(
            "- runFlow:\n    file: a.yaml\n    env:\n      CODE: \"{env_val}\"\n"
        ))
    };
    assert_ne!(flow("1"), flow("2"));
    assert_eq!(
        flow("1"),
        vec!["|runFlow|params={\"env\":{\"CODE\":\"1\"}}"]
    );

    assert_eq!(
        entries("appId: a\nenv:\n  NAME: x\n---\n- assertVisible: \"${NAME}\"\n"),
        vec!["|env|{\"NAME\":\"x\"}", "|assertVisible|\"${NAME}\""]
    );
    // Cosmetic label is ignored in params.
    assert_eq!(
        entries("- runFlow:\n    file: a.yaml\n    label: One\n"),
        entries("- runFlow:\n    file: a.yaml\n    label: Two\n")
    );
}

#[test]
fn counts_inline_scripts_and_reports_which_lines_always_run() {
    let yaml = [
        "- evalScript: ${output.ok = true}",
        "- assertTrue: ${output.ok}",
        "- assertVisible:",
        "    id: \"soft\"",
        "    optional: true",
        "- runFlow:",
        "    file: a.yaml",
        "    when:",
        "      platform: iOS",
        "- runFlow: b.yaml",
        "- runScript: c.js",
        "- repeat:",
        "    times: 1",
        "    commands:",
        "      - assertVisible: \"inside\"",
        "",
    ]
    .join("\n");
    let items = find_flow_assertions(&yaml).expect("valid yaml");
    let summary: Vec<(&str, bool)> = items
        .iter()
        .map(|i| (i.command.as_str(), i.always))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("evalScript", true),
            ("assertTrue", true),
            ("assertVisible", false),
            ("runFlow", false),
            ("runFlow", true),
            ("runScript", true),
            ("repeat", true),
            ("assertVisible", false),
        ]
    );
    assert_eq!(items[0].entry, "|evalScript|\"${output.ok = true}\"");

    let calls: Vec<(FlowAssertionCallKind, &str)> = items
        .iter()
        .filter_map(|i| i.call.as_ref().map(|c| (c.kind, c.path.as_str())))
        .collect();
    assert_eq!(
        calls,
        vec![
            (FlowAssertionCallKind::Flow, "a.yaml"),
            (FlowAssertionCallKind::Flow, "b.yaml"),
            (FlowAssertionCallKind::Script, "c.js"),
        ]
    );

    let on_start = find_flow_assertions("appId: a\nonFlowStart:\n  - assertVisible: \"x\"\n")
        .expect("valid yaml");
    assert_eq!(
        on_start.iter().map(|i| i.always).collect::<Vec<_>>(),
        vec![true]
    );
}

#[test]
fn bare_word_assertion_and_extended_wait_until() {
    let yaml = "- assertNoDefectsWithAi\n- tapOn: x\n- extendedWaitUntil:\n    visible: \"Done\"\n    timeout: 5000\n";
    let items = find_flow_assertions(yaml).expect("valid yaml");
    assert_eq!(
        items.iter().map(|i| i.entry.as_str()).collect::<Vec<_>>(),
        vec![
            "|assertNoDefectsWithAi|null",
            "|extendedWaitUntil|{\"timeout\":5000,\"visible\":\"Done\"}",
        ]
    );
    assert_eq!(
        items.iter().map(|i| i.command.as_str()).collect::<Vec<_>>(),
        vec!["assertNoDefectsWithAi", "extendedWaitUntil"]
    );
    assert_ne!(
        entries("- extendedWaitUntil:\n    notVisible: \"Done\"\n    timeout: 5000\n"),
        entries("- extendedWaitUntil:\n    visible: \"Done\"\n    timeout: 5000\n")
    );
    assert_ne!(
        entries("- extendedWaitUntil:\n    visible: \"Done\"\n    timeout: 1\n"),
        entries("- extendedWaitUntil:\n    visible: \"Done\"\n    timeout: 5000\n")
    );
}

#[test]
fn optional_counts_however_yaml_spells_it_including_through_merge_key() {
    let always = |yaml: &str| {
        find_flow_assertions(yaml)
            .expect("valid yaml")
            .into_iter()
            .map(|i| (i.command, i.always))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        always("- assertVisible:\n    id: a\n    optional: true\n"),
        vec![("assertVisible".to_string(), false)]
    );
    assert_eq!(
        always("- assertVisible:\n    id: a\n    optional: yes\n"),
        vec![("assertVisible".to_string(), false)]
    );
    assert_eq!(
        always("- assertVisible:\n    id: a\n    optional: false\n"),
        vec![("assertVisible".to_string(), true)]
    );
    assert_eq!(
        always(
            "- defaults: &base\n    optional: true\n- assertVisible:\n    <<: *base\n    id: a\n"
        ),
        vec![("assertVisible".to_string(), false)]
    );
    assert_eq!(
        always("- assertVisible:\n    <<: {optional: true}\n    id: a\n"),
        vec![("assertVisible".to_string(), false)]
    );
    assert_eq!(
        always("- runFlow:\n    file: a.yaml\n    optional: true\n"),
        vec![("runFlow".to_string(), false)]
    );
    assert_eq!(
        always("- runFlow:\n    file: a.yaml\n    optional: yes\n"),
        vec![("runFlow".to_string(), false)]
    );
    assert_eq!(
        always("- runFlow:\n    <<: {optional: true}\n    file: a.yaml\n"),
        vec![("runFlow".to_string(), false)]
    );
    assert_eq!(
        always("- runFlow:\n    optional: true\n    commands:\n      - assertVisible: a\n"),
        vec![
            ("runFlow".to_string(), false),
            ("assertVisible".to_string(), false),
        ]
    );
}

#[test]
fn finds_assertions_in_every_document_and_on_flow_start() {
    let yaml =
        "appId: a\nonFlowStart:\n  - assertVisible: \"start\"\n---\n- assertVisible: \"main\"\n";
    assert_eq!(
        entries(yaml),
        vec![
            "onFlowStart|assertVisible|\"start\"",
            "|assertVisible|\"main\"",
        ]
    );
}

#[test]
fn repeats_are_kept_and_unreadable_yaml_errors() {
    assert_eq!(
        entries("- assertVisible: \"a\"\n- assertVisible: \"a\"\n").len(),
        2
    );
    assert!(find_flow_assertions("a: [unclosed").is_err());
}

#[test]
fn exposes_id_of_assert_visible() {
    let ids: Vec<Option<String>> =
        find_flow_assertions("- assertVisible:\n    id: \"x\"\n- assertVisible: \"text\"\n")
            .expect("valid yaml")
            .into_iter()
            .map(|a| a.id)
            .collect();
    assert_eq!(ids, vec![Some("x".to_string()), None]);
}

// =============================================================================================
// All 29 checked-in YAML files across demo/live-evidence/**, spikes/**, and .github/workflows/*.yml

#[test]
fn all_checked_in_repo_yamls_match_typescript_sha256_goldens() {
    let root = repo_root();
    let expected: &[(&str, usize, &str, &str)] = &[
        (
            "demo/live-evidence/android/cold-saved-checkout.yaml",
            14,
            "372ae8399c014b847a7a5e4408141350ee2c2c29f1100c747ee5bb2259815c4d",
            "ba0b0c4f8a740129e5a8532212d473064c8549ff84025fe89830433840c861b6",
        ),
        (
            "demo/live-evidence/android/repair-baseline-saved-checkout.yaml",
            12,
            "24bb2d1bd828cc0eb87a8500f360dd1b4493168fc1f24eec5ee293d79b1fab22",
            "6edeee3dab0142fe5264112399a4a0314e8be80b5b1139d173230ca448839a85",
        ),
        (
            "demo/live-evidence/android/repair-warm-saved-checkout.yaml",
            12,
            "24bb2d1bd828cc0eb87a8500f360dd1b4493168fc1f24eec5ee293d79b1fab22",
            "2afa74125afae0af66eaea235544a726ef9f01f17b869b507e721c65cfaebc14",
        ),
        (
            "demo/live-evidence/android/replay-saved-checkout.yaml",
            12,
            "24bb2d1bd828cc0eb87a8500f360dd1b4493168fc1f24eec5ee293d79b1fab22",
            "778cbf8b4b0639a0bf5c6b96a7ebb8e2790798601a4b93bab9927ab7b453f3f4",
        ),
        (
            "demo/live-evidence/android/reviewed-model-authored-checkout.yaml",
            14,
            "372ae8399c014b847a7a5e4408141350ee2c2c29f1100c747ee5bb2259815c4d",
            "57e74533a867103658040771345c3f4e43d866fa65c390f39133d8b9031bb5dd",
        ),
        (
            "demo/live-evidence/android/reviewed-repair-fixture.yaml",
            12,
            "24bb2d1bd828cc0eb87a8500f360dd1b4493168fc1f24eec5ee293d79b1fab22",
            "0d7497cce67a57370a9230f33121c5d1369a4f4afa343e56e9b83a445682c5d3",
        ),
        (
            "demo/live-evidence/android/stale-repair-saved-checkout.yaml",
            12,
            "24bb2d1bd828cc0eb87a8500f360dd1b4493168fc1f24eec5ee293d79b1fab22",
            "c425b50e1840d86508ee48af92c7d83e51c6a868c215dd68ec35f5c2ca1ac33a",
        ),
        (
            "demo/live-evidence/android/warm-saved-checkout.yaml",
            14,
            "372ae8399c014b847a7a5e4408141350ee2c2c29f1100c747ee5bb2259815c4d",
            "77a279a4fba0cf31ea2706b15434db87e8a5ca194f0d85492780745bd5a87748",
        ),
        (
            "demo/live-evidence/chromium/remember-saved-checkout.yaml",
            12,
            "30644532abe48887f74ccf2ab27369b9e2af9aa452135a5d03b1f13c389c059f",
            "3d30c651a16d1d9d0d6f3fd9454e0572d84bcabd050e1cfb8479197525b635e7",
        ),
        (
            "demo/live-evidence/chromium/reviewed-checkout.yaml",
            12,
            "30644532abe48887f74ccf2ab27369b9e2af9aa452135a5d03b1f13c389c059f",
            "a1f1611b119666796929f1357ad1c51edf5e968a95c8ad64d9c5e7f06b097f26",
        ),
        (
            "demo/live-evidence/chromium/run-saved-checkout.yaml",
            12,
            "30644532abe48887f74ccf2ab27369b9e2af9aa452135a5d03b1f13c389c059f",
            "b583de1d8864afcade2d3877606ae95a43e44b9e99d175958cc77490cfdcce74",
        ),
        (
            "spikes/001-android-deja/evidence/agent-cold-checkout.yaml",
            12,
            "1ea9af1fdfae8fc85b9089b6029ff5cbbff024dc474df1097794c6e08ac07e26",
            "249918e8004af53b044dd8385487c247653195f5ae275b41ecc24e5cbf35d131",
        ),
        (
            "spikes/001-android-deja/evidence/explore-candidate.yaml",
            12,
            "1b8f42acbb0a79c9b08fdd0ca47000bf67e646dae7a22f00f1ac4afcd7b49f0c",
            "c29889005eed22e29a277853d4f40ab36bf5af1ff3404ced4481f8cc4986e69a",
        ),
        (
            "spikes/001-android-deja/evidence/explore-rejected-candidate.yaml",
            2,
            "0898478b08f7bad01f586124e925855027dccfa00a8086fff4aaea01bdeeccee",
            "a1af6369a6b7f71ce092dec6c8e34b09815bfb8b1811d81ea38e867ced62b532",
        ),
        (
            "spikes/001-android-deja/evidence/repaired-checkout.yaml",
            12,
            "1ea9af1fdfae8fc85b9089b6029ff5cbbff024dc474df1097794c6e08ac07e26",
            "8b9eb244eb3c9126e66c94a6cd89b7a678eba2313bae4aeda942370a089ee104",
        ),
        (
            "spikes/001-android-deja/evidence/walk018/composed/sign-in-and-check-out-mochi-sticker-pack/candidate.yaml",
            10,
            "d8630703318fbfef7e783b986d3902bc0b9ab61f8af45b19285436cba10fc5f5",
            "fcf4a278cd225fbc5e0e0184f31edf9fe8d32a4fac0cb0e3c59a3eb7853a1a9f",
        ),
        (
            "spikes/001-android-deja/evidence/walk018/continuation/checkout.yaml",
            9,
            "01c6c1933ba9e4a29831dea6f7d460e567a7621093b81444bdf5eacf06f5349a",
            "4ccd3fc1d91caa801fdb48d75d5befe74eec0248762906f019df1b661dd64afe",
        ),
        (
            "spikes/001-android-deja/evidence/walk018/segments/login-v1/android-c415a62dae8b1346213ac1f92c5201d2a8c17e01d98c84e6ab979f4d785f3266.yaml",
            5,
            "d07aace207ebe6868dae2d98a17be512f3adbba3d86b42bc6d574ad146894205",
            "90faa7a8c1979ab922bf7c747cee6b1088197be00187eec2720d9f837ebebbdf",
        ),
        (
            "spikes/001-android-deja/flows/checkout.yaml",
            5,
            "eb1c3b791938b9144023cef62b2a997ddbb4d04a8f28d3707ac8f70ae7edfb81",
            "b07ac83240cfa7cefaa1d7f6d49018484a52e0e7c20ddb2b02aec846aa28c4ac",
        ),
        (
            "spikes/001-android-deja/flows/product-regression.yaml",
            12,
            "ae8f25faf716baa09d7935f0c33736b83b05673f4565f23ee0f4a816ef23821e",
            "2e09e28a0c2e6138def36cbe8bc12d54882cecf951930e3e4a5903bbac40250f",
        ),
        (
            "spikes/001-android-deja/flows/repaired-checkout.yaml",
            12,
            "4eb63396586410f06bea93f0369d8d11a0b1be478ac77adbb1920549bd7095c1",
            "b760a17e3c1830eb989e48c58cbe6cc081ff708846a6731deff229bad91808b7",
        ),
        (
            "spikes/002-web-deja/evidence/checkout.yaml",
            12,
            "30644532abe48887f74ccf2ab27369b9e2af9aa452135a5d03b1f13c389c059f",
            "9d939ddd92e1fa156b8023d2b972137c12528c2d56efd91048113722fa7f91a5",
        ),
        (
            "spikes/002-web-deja/flows/checkout.yaml",
            12,
            "d03160603c3a923c96ad32c19f0c065298356a3f0658aac8ca93a32e5fb21813",
            "6a95a1211e94fef99115506ec1ff21da2ac18759510fd225c752bf20058e4650",
        ),
        (
            "spikes/003-ios-deja/evidence/checkout.yaml",
            12,
            "5991c501226e09d68e63f310d5acbe9734eccd519dc6119b7968419bde4238f3",
            "28b3abee4d2963b626538fb156dbe5423d38ffceb432d737dbd8890e8c990c68",
        ),
        (
            "spikes/003-ios-deja/flows/checkout.yaml",
            12,
            "dfbb444ab4bebbb44863d6001d330a864422f958084c14adc2dd1078ecd490f9",
            "fb5af41a3c35c8cb8ecf73e398a250c4876e5d6f9ee2558b4d99b7434d1dee51",
        ),
    ];

    for &(rel_path, expected_count, expected_file_hash, expected_closure_hash) in expected {
        let full = root.join(rel_path);
        let contents =
            fs::read_to_string(&full).unwrap_or_else(|e| panic!("failed to read {rel_path}: {e}"));
        let items = find_flow_assertions(&contents)
            .unwrap_or_else(|e| panic!("find_flow_assertions failed on {rel_path}: {e}"));
        assert_eq!(items.len(), expected_count, "count mismatch for {rel_path}");
        assert_eq!(
            single_file_assertions_hash(&contents),
            expected_file_hash,
            "file assertion hash mismatch for {rel_path}"
        );

        let closure = collect_flow_closure(&root, &full)
            .unwrap_or_else(|e| panic!("collect_flow_closure failed on {rel_path}: {e}"));
        let actual_closure_hash = assertions_sha256(&closure)
            .unwrap_or_else(|e| panic!("assertions_sha256 failed on {rel_path}: {e}"));
        assert_eq!(
            actual_closure_hash, expected_closure_hash,
            "closure assertionsSha256 mismatch for {rel_path}"
        );
    }
}

// =============================================================================================
// Synthetic YAML 1.2 + `<<` merge + ES262 float goldens

#[test]
fn synthetic_yaml12_merge_and_es262_float_goldens() {
    let merge_quoted_key = "- assertVisible:\n    \"<<\": { optional: true }\n    id: a\n";
    assert_eq!(
        single_file_assertions_hash(merge_quoted_key),
        "9318d0fe39d2ce9813ddc5e7173a5048a5cddfbb88ec69766b9f296ebc81380f"
    );

    let merge_str_tag = "- assertVisible:\n    !!str \"<<\": { optional: true }\n    id: a\n";
    assert_eq!(
        single_file_assertions_hash(merge_str_tag),
        "9318d0fe39d2ce9813ddc5e7173a5048a5cddfbb88ec69766b9f296ebc81380f"
    );

    let yaml12_scalars = concat!(
        "appId: com.example\n---\n- assertVisible:\n",
        "    yesWord: yes\n",
        "    noWord: no\n",
        "    onWord: on\n",
        "    offWord: off\n",
        "    t1: true\n",
        "    t2: True\n",
        "    t3: TRUE\n",
        "    f1: false\n",
        "    f2: False\n",
        "    f3: FALSE\n",
        "    n1: ~\n",
        "    n2: null\n",
        "    n3: Null\n",
        "    n4: NULL\n",
        "    n5:\n",
        "    oct: 0o10\n",
        "    hex: 0x10\n",
        "    under: 1_000\n",
        "    negZero: -0.0\n",
        "    infPos: .inf\n",
        "    infNeg: -.inf\n",
        "    notNum: .nan\n",
        "    e21: 1e21\n",
        "    e20: 1e20\n",
        "    em6: 1e-6\n",
        "    em7: 1e-7\n",
        "    tagStr: !!str 123\n",
        "    tagBoolBad: !!bool yes\n",
        "    tagIntBad: !!int nope\n",
        "    tagFloatInt: !!float 10\n",
        "    tagFloatDec: !!float 1.5\n",
        "    tagNullBad: !!null foo\n",
    );
    assert_eq!(
        single_file_assertions_hash(yaml12_scalars),
        "2215934408792ea6c7064eb1fac86fc0f2a8f9a732cd5feda7c0a36337b2ff1d"
    );

    let merge_precedence = "- defs:\n    a: &a { x: 1, y: 2 }\n    b: &b { y: 3, z: 4 }\n- assertVisible:\n    x: 10\n    <<: [*a, *b]\n    z: 40\n";
    assert_eq!(
        single_file_assertions_hash(merge_precedence),
        "856404b41fba07d4d9adf008505f8206355db31775ffd6a20c89ee178ec35aa2"
    );

    let merge_multiple_plain = "- defs:\n    a: &a { x: 1, y: 2 }\n    b: &b { y: 3, z: 4 }\n- assertVisible:\n    <<: *a\n    <<: *b\n";
    assert_eq!(
        single_file_assertions_hash(merge_multiple_plain),
        "f3a2d3c358921d9f8384234eb1431f72fdb444605391ec10883c5c020e07852f"
    );

    let merge_assert_inside_alias =
        "- defs: &base\n    assertVisible:\n      id: from_alias\n- <<: *base\n";
    let alias_items = find_flow_assertions(merge_assert_inside_alias).expect("valid yaml");
    assert_eq!(
        alias_items.iter().map(|i| i.always).collect::<Vec<_>>(),
        vec![false, true]
    );
    assert_eq!(
        single_file_assertions_hash(merge_assert_inside_alias),
        "a2470883c5a8e45594b9abcdb0c30c2dc3975817fb023dd639c699e6f1c04eae"
    );

    let js_key_order = "- assertVisible:\n    b: 1\n    \"10\": ten\n    \"2\": two\n    \"0\": zero\n    \"-1\": neg\n    a: 2\n    \"4294967294\": maxIdx\n    \"4294967295\": notIdx\n    \"01\": leadingZero\n";
    assert_eq!(
        single_file_assertions_hash(js_key_order),
        "c24c30cbaea1b1b0385a996e65a6b19da7732055f98eee1e021907b0d388f6b5"
    );
}

#[test]
fn yaml_loader_edge_cases_and_security_caps() {
    assert_eq!(parse_yaml_documents("").expect("empty"), vec![]);
    assert_eq!(
        parse_yaml_documents("# comment only\n# line 2\n").expect("comments"),
        vec![]
    );
    assert_eq!(
        parse_yaml_documents("---\n").expect("empty doc"),
        vec![YamlValue::Null]
    );
    assert_eq!(
        parse_yaml_documents("---\n---\n").expect("two empty docs"),
        vec![YamlValue::Null, YamlValue::Null]
    );

    // Duplicate keys fail closed (including numeric normalization and NaN).
    assert!(parse_yaml_documents("a: 1\na: 2\n").is_err());
    assert!(parse_yaml_documents("1: a\n1.0: b\n").is_err());
    assert!(parse_yaml_documents("0: a\n-0.0: b\n").is_err());
    assert!(parse_yaml_documents(".nan: a\n.nan: b\n").is_err());
    assert!(parse_yaml_documents("null: a\n~: b\n").is_err());
    assert!(parse_yaml_documents(".inf: a\n+.inf: b\n").is_err());

    // Distinct key types pass uniqueKeys; later key overwrites in JS object representation.
    let distinct = parse_yaml_documents("1: a\n\"1\": b\n").expect("Number(1) != String(\"1\")");
    assert_eq!(
        distinct,
        vec![YamlValue::Map(vec![(
            "1".to_string(),
            YamlValue::String("b".to_string())
        )])]
    );

    // JS Object.entries key order preserved (array indices ascending first, then insertion order).
    let ordered = parse_yaml_documents(
        "b: 1\n\"10\": ten\n\"2\": two\n\"0\": zero\n\"-1\": neg\na: 2\n\"4294967294\": maxIdx\n\"4294967295\": notIdx\n\"01\": leadingZero\n",
    )
    .expect("valid map");
    let keys: Vec<&str> = ordered[0]
        .as_map()
        .expect("map")
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![
            "0",
            "2",
            "10",
            "4294967294",
            "b",
            "-1",
            "a",
            "4294967295",
            "01"
        ]
    );

    // Non-map merge fails closed.
    assert!(parse_yaml_documents("<<: 123\n").is_err());
    assert!(parse_yaml_documents("<<: [{ a: 1 }, 123]\n").is_err());

    // Cyclic alias fails closed.
    assert!(parse_yaml_documents("a: &a\n  b: *a\n").is_err());

    // Billion-laughs DoS bomb hits MAX_ALIAS_EXPANSIONS cap.
    assert_eq!(MAX_ALIAS_EXPANSIONS, 100_000);
    assert_eq!(MAX_MATERIALIZED_BYTES, 16 * 1024 * 1024);
    let bomb = [
        "a: &a [1, 1, 1, 1, 1, 1, 1, 1, 1, 1]",
        "b: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]",
        "c: &c [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]",
        "d: &d [*c, *c, *c, *c, *c, *c, *c, *c, *c, *c]",
        "e: &e [*d, *d, *d, *d, *d, *d, *d, *d, *d, *d]",
        "f: [*e, *e, *e, *e, *e, *e, *e, *e, *e, *e]",
        "",
    ]
    .join("\n");
    assert!(parse_yaml_documents(&bomb).is_err());

    // Repeated alias of a large mapping fails cleanly on node/byte budgets before OOM.
    let mut big_map = String::from("base: &big\n");
    let big_val = "z".repeat(4096);
    for i in 0..128 {
        big_map.push_str(&format!("  k{i}: \"{big_val}\"\n"));
    }
    big_map.push_str("copies:\n");
    for _ in 0..64 {
        big_map.push_str("  - *big\n");
    }
    assert!(
        parse_yaml_documents(&big_map).is_err(),
        "repeated alias of large mapping should fail closed on byte budget"
    );

    // Explicitly tagged `!!merge <<` and full core URI expand like plain `<<`.
    let merge_tagged = "- assertVisible:\n    !!merge <<: { id: ok }\n";
    let merge_full = "- assertVisible:\n    !<tag:yaml.org,2002:merge> <<: { id: ok }\n";
    assert_eq!(
        entries(merge_tagged),
        vec!["|assertVisible|{\"id\":\"ok\"}"]
    );
    assert_eq!(entries(merge_full), vec!["|assertVisible|{\"id\":\"ok\"}"]);

    // Collection keys with embedded commas/colons remain distinct from multi-element collections,
    // while preserving original YAML scalar style matching yaml@2.9.1.
    let coll_plain = "[foo, bar]: 1\n";
    assert_eq!(
        parse_yaml_documents(coll_plain).expect("plain seq key")[0]
            .as_map()
            .expect("map")[0]
            .0,
        "[ foo, bar ]"
    );

    let coll_single = "['foo', 'a, b']: 1\n";
    assert_eq!(
        parse_yaml_documents(coll_single).expect("single-quoted seq key")[0]
            .as_map()
            .expect("map")[0]
            .0,
        "[ 'foo', 'a, b' ]"
    );

    let coll_keys = "[\"a, b\"]: 1\n[\"a\", \"b\"]: 2\n";
    let coll_docs = parse_yaml_documents(coll_keys).expect("distinct collection keys");
    let coll_entries = coll_docs[0].as_map().expect("map");
    assert_eq!(coll_entries.len(), 2);
    assert_eq!(coll_entries[0].0, "[ \"a, b\" ]");
    assert_eq!(coll_entries[1].0, "[ \"a\", \"b\" ]");

    let map_coll_plain = "{a: foo, b: bar}: 1\n";
    assert_eq!(
        parse_yaml_documents(map_coll_plain).expect("plain map key")[0]
            .as_map()
            .expect("map")[0]
            .0,
        "{ a: foo, b: bar }"
    );

    let map_coll_keys = "{a: \"1, b: 2\"}: 1\n{a: 1, b: 2}: 2\n";
    let map_coll_docs = parse_yaml_documents(map_coll_keys).expect("distinct map collection keys");
    let map_coll_entries = map_coll_docs[0].as_map().expect("map");
    assert_eq!(map_coll_entries.len(), 2);
    assert_eq!(map_coll_entries[0].0, "{ a: \"1, b: 2\" }");
    assert_eq!(map_coll_entries[1].0, "{ a: 1, b: 2 }");

    // Deeply nested unused anchors over a large scalar use O(1) Rc cloning without quadratic blowup.
    let mut nested_anchors = String::new();
    for i in 0..80 {
        nested_anchors.push_str(&"  ".repeat(i));
        nested_anchors.push_str(&format!("k{i}: &a{i}\n"));
    }
    nested_anchors.push_str(&"  ".repeat(80));
    nested_anchors.push_str(&format!("leaf: \"{}\"\n", "x".repeat(65_536)));
    assert!(
        parse_yaml_documents(&nested_anchors).is_ok(),
        "deeply nested unused anchors should parse cleanly via Rc<RawNode>"
    );

    // %YAML 1.2 is accepted; %YAML 1.1 (and other non-1.2 versions) fail closed.
    let yaml12 = parse_yaml_documents("%YAML 1.2\n---\na: yes\n").expect("YAML 1.2 allowed");
    assert_eq!(yaml12[0].get("a"), Some(&YamlValue::String("yes".into())));
    let yaml11_err = parse_yaml_documents("%YAML 1.1\n---\na: yes\n").unwrap_err();
    assert!(yaml11_err.to_string().contains("unsupported YAML version"));
    let yaml11_second_doc =
        parse_yaml_documents("a: 1\n...\n%YAML 1.1\n---\nb: yes\n").unwrap_err();
    assert!(
        yaml11_second_doc
            .to_string()
            .contains("unsupported YAML version")
    );
}

#[test]
fn find_flow_assertions_enforces_byte_budget_on_inline_run_flow() {
    // An inline runFlow carrying a 200 KiB env value repeated into 100 child assertions would
    // materialize >20 MiB of entries; find_flow_assertions must fail closed before OOM.
    let pad = "p".repeat(200 * 1024);
    let mut yaml = format!("- runFlow:\n    env:\n      PAD: \"{pad}\"\n    commands:\n");
    for i in 0..100 {
        yaml.push_str(&format!(
            "      - assertVisible:\n          id: \"id.{i}\"\n"
        ));
    }
    let err = match find_flow_assertions(&yaml) {
        Ok(items) => panic!("expected byte cap error, got {} items", items.len()),
        Err(err) => err,
    };
    assert!(err.to_string().contains("bytes"));
}

#[test]
fn es262_number_to_string_parity() {
    let cases: &[(f64, &str, &str)] = &[
        (0.0, "0", "0"),
        (-0.0, "0", "0"),
        (1.0, "1", "1"),
        (-1.0, "-1", "-1"),
        (0.5, "0.5", "0.5"),
        (1e20, "100000000000000000000", "100000000000000000000"),
        (1e21, "1e+21", "1e+21"),
        (1.5e20, "150000000000000000000", "150000000000000000000"),
        (1.5e21, "1.5e+21", "1.5e+21"),
        (1e-6, "0.000001", "0.000001"),
        (1e-7, "1e-7", "1e-7"),
        (1.5e-6, "0.0000015", "0.0000015"),
        (1.5e-7, "1.5e-7", "1.5e-7"),
        (5e-6, "0.000005", "0.000005"),
        (5e-7, "5e-7", "5e-7"),
        (
            9_007_199_254_740_991.0,
            "9007199254740991",
            "9007199254740991",
        ),
        (0.1 + 0.2, "0.30000000000000004", "0.30000000000000004"),
        (1e-308, "1e-308", "1e-308"),
        (1e308, "1e+308", "1e+308"),
        (5e-324, "5e-324", "5e-324"),
        (f64::NAN, "NaN", "null"),
        (f64::INFINITY, "Infinity", "null"),
        (f64::NEG_INFINITY, "-Infinity", "null"),
    ];
    for &(n, expected_str, expected_json) in cases {
        assert_eq!(
            js_number_to_string(n),
            expected_str,
            "js_number_to_string({n})"
        );
        assert_eq!(
            js_json_number_to_string(n),
            expected_json,
            "js_json_number_to_string({n})"
        );
    }
}

// =============================================================================================
// Multi-file consumer-flow.test.ts closure golden

#[test]
fn consumer_flow_multi_file_closure_and_assertions_golden() {
    let main = "appId: com.example\n---\n- assertVisible:\n    id: \"screen.today\"\n- runFlow: ../subflows/sub.yaml\n- runScript: ../subflows/check.js\n";
    let sub = "- assertNotVisible:\n    id: \"today.error\"\n";
    let script = "output.ok = true\n";

    let tmp = TempDir::new("consumer-flow");
    tmp.write(".maestro/flows/main.yaml", main);
    tmp.write(".maestro/subflows/sub.yaml", sub);
    tmp.write(".maestro/subflows/check.js", script);

    let closure = collect_flow_closure(tmp.path(), &tmp.path().join(".maestro/flows/main.yaml"))
        .expect("closure should succeed");
    assert_eq!(
        closure.files.iter().map(|f| f.kind).collect::<Vec<_>>(),
        vec![
            FlowReferenceKind::Flow,
            FlowReferenceKind::Script,
            FlowReferenceKind::Flow,
        ]
    );
    assert_eq!(
        closure.sha256,
        "0390f482a200fb8e79b697e5d9cd8afaf313ed0c911aceb651762c1746e254d3"
    );

    let lines: Vec<String> = closure_assertions(&closure)
        .expect("closure assertions")
        .into_iter()
        .map(|a| a.entry)
        .collect();
    assert_eq!(
        lines,
        vec![
            "flows/main.yaml|||assertVisible|{\"id\":\"screen.today\"}",
            "flows/main.yaml|||runFlow|params={}",
            "subflows/sub.yaml|flows/main.yaml|||runFlow|params={}||assertNotVisible|{\"id\":\"today.error\"}",
            "flows/main.yaml|||runScript|params={}",
            "flows/main.yaml|||runScript|params={}|script=subflows/check.js|sha256=1bd1958618755ba737b2107d401518d9efac80e6baaf85b3a46c54ac44a6c6a9",
        ]
    );

    assert_eq!(
        assertions_sha256(&closure).expect("assertions sha256"),
        "67aa68ae3df829493086e2b37a13c8663d4634976c7f545c91493dea7b64d2aa"
    );

    // Optional runFlow in any spelling (including through merge key) sets reached_always = false.
    let check_reached = |call: &str| {
        let sub_tmp = TempDir::new("reached");
        sub_tmp.write(
            ".maestro/subflows/sub.yaml",
            "- assertVisible:\n    id: saved\n",
        );
        sub_tmp.write(
            ".maestro/flows/main.yaml",
            &format!("appId: a\n---\n{call}"),
        );
        let c = collect_flow_closure(
            sub_tmp.path(),
            &sub_tmp.path().join(".maestro/flows/main.yaml"),
        )
        .expect("closure");
        closure_assertions(&c)
            .expect("assertions")
            .into_iter()
            .find(|i| i.command == "assertVisible")
            .expect("assertVisible present")
            .reached_always
    };
    assert!(check_reached("- runFlow: ../subflows/sub.yaml\n"));
    assert!(check_reached(
        "- runFlow:\n    file: ../subflows/sub.yaml\n    optional: false\n"
    ));
    assert!(!check_reached(
        "- runFlow:\n    file: ../subflows/sub.yaml\n    optional: true\n"
    ));
    assert!(!check_reached(
        "- runFlow:\n    file: ../subflows/sub.yaml\n    optional: yes\n"
    ));
    assert!(!check_reached(
        "- defaults: &opt\n    optional: true\n- runFlow:\n    <<: *opt\n    file: ../subflows/sub.yaml\n"
    ));
    assert!(!check_reached(
        "- runFlow:\n    <<: {optional: true}\n    file: ../subflows/sub.yaml\n"
    ));
}

#[test]
fn yaml_round3_stream_budget_tags_collection_keys_and_radix_single_rounding() {
    use deja_core::{CoreError, YamlValue, parse_yaml_documents};

    // 1. Large hex integer rounds strictly once to IEEE-754 f64 (matching yaml@2.9.1).
    let docs = parse_yaml_documents("0x100000000000008000000000000000001\n").expect("large hex");
    assert_eq!(docs.len(), 1);
    match docs[0] {
        YamlValue::Number(n) => assert_eq!(n, 3.402_823_669_209_385_4e38_f64),
        ref other => panic!("expected Number, got {other:?}"),
    }

    // 2. Styled collection keys `[foo]` and `['foo']` remain distinct like yaml@2.9.1.
    let styled = parse_yaml_documents("{ [foo]: 1, ['foo']: 2 }\n").expect("distinct styled keys");
    assert_eq!(styled.len(), 1);
    match &styled[0] {
        YamlValue::Map(entries) => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].0, "[ foo ]");
            assert_eq!(entries[1].0, "[ 'foo' ]");
        }
        other => panic!("expected Map, got {other:?}"),
    }

    // 3. Unsupported non-core YAML tags (e.g. !!timestamp) fail explicitly instead of hashing wrong.
    let err = parse_yaml_documents("when: !!timestamp 2024-01-02\n")
        .expect_err("unsupported !!timestamp must fail");
    assert!(matches!(err, CoreError::YamlParse(ref m) if m.contains("unsupported YAML tag")));

    // 4. Multi-document YAML stream shares one aggregate materialization budget.
    let doc_chunk = format!(
        "---\na: &a \"{}\"\nb: [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\n",
        "x".repeat(50_000)
    );
    let stream = doc_chunk.repeat(20);
    let budget_err = parse_yaml_documents(&stream)
        .expect_err("multi-document stream exceeding MAX_MATERIALIZED_BYTES must fail");
    assert!(
        matches!(budget_err, CoreError::YamlParse(ref m) if m.contains("materialized size")),
        "unexpected error: {budget_err:?}"
    );
}
