use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

const EXPECTED_CRATES: &[&str] = &[
    "deja-core",
    "deja-store",
    "deja-flow",
    "deja-driver-ios",
    "deja-driver-android",
    "deja-driver-web",
    "deja-explore",
    "deja-mcp",
    "deja-cli",
];

const DRIVER_CRATES: &[&str] = &["deja-driver-ios", "deja-driver-android", "deja-driver-web"];

/// Closed-world allowlist of direct external normal/build dependencies for `deja-core`.
/// Any HTTP, subprocess, SQLite, async runtime, or driver crate added to `deja-core`
/// fails closed here without needing to enumerate every ecosystem crate name.
const ALLOWED_CORE_DIRECT_DEPS: &[&str] = &[
    "hex",
    "regex",
    "saphyr",
    "saphyr-parser",
    "serde",
    "serde_json",
    "sha2",
    "thiserror",
    "unicode-normalization",
];

/// Closed-world allowlist of direct external dev-dependencies for `deja-core`.
const ALLOWED_CORE_DIRECT_DEV_DEPS: &[&str] = &[
    "hex",
    "regex",
    "saphyr",
    "saphyr-parser",
    "serde",
    "serde_json",
    "sha2",
    "tempfile",
    "thiserror",
    "unicode-normalization",
];

const GLOBAL_FORBIDDEN_EXTERNAL: &[&str] = &[
    "openssl",
    "openssl-sys",
    "native-tls",
    "native-tls-crate",
    "hyper-tls",
    "tokio-native-tls",
];

const DIRECT_OWNER_RULES: &[(&str, &[&str])] = &[
    (
        "deja-store",
        &["rusqlite", "libsqlite3-sys", "sqlite-wasm-rs"],
    ),
    ("deja-driver-web", &["tokio-tungstenite", "tungstenite"]),
    ("deja-cli", &["clap", "tracing-subscriber"]),
];

#[derive(Debug, Clone)]
struct CrateDeps {
    direct_normal_and_build: BTreeSet<String>,
    direct_dev: BTreeSet<String>,
    transitive_normal_and_build: BTreeSet<String>,
    transitive_dev: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceSnapshot {
    crates: BTreeMap<String, CrateDeps>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
    resolve: Option<MetadataResolve>,
}

#[derive(Debug, Deserialize)]
struct MetadataPackage {
    id: String,
    name: String,
    dependencies: Vec<MetadataDependency>,
}

#[derive(Debug, Deserialize)]
struct MetadataDependency {
    name: String,
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MetadataResolve {
    nodes: Vec<ResolveNode>,
}

#[derive(Debug, Deserialize)]
struct ResolveNode {
    id: String,
    deps: Vec<ResolveDep>,
}

#[derive(Debug, Deserialize)]
struct ResolveDep {
    pkg: String,
    dep_kinds: Vec<DepKindInfo>,
}

#[derive(Debug, Deserialize)]
struct DepKindInfo {
    kind: Option<String>,
}

fn allowed_internal_deps(crate_name: &str) -> Option<&'static [&'static str]> {
    match crate_name {
        "deja-core" => Some(&[]),
        "deja-store" | "deja-flow" => Some(&["deja-core"]),
        "deja-driver-ios" | "deja-driver-android" | "deja-driver-web" => {
            Some(&["deja-core", "deja-flow"])
        }
        "deja-explore" => Some(&["deja-core", "deja-store", "deja-flow"]),
        "deja-mcp" => Some(&["deja-core", "deja-store", "deja-flow", "deja-explore"]),
        "deja-cli" => Some(&[
            "deja-core",
            "deja-store",
            "deja-flow",
            "deja-driver-ios",
            "deja-driver-android",
            "deja-driver-web",
            "deja-explore",
            "deja-mcp",
        ]),
        _ => None,
    }
}

fn forbidden_external_all(crate_name: &str) -> &'static [&'static str] {
    match crate_name {
        "deja-core" => &[
            "rusqlite",
            "libsqlite3-sys",
            "sqlite-wasm-rs",
            "reqwest",
            "hyper",
            "h2",
            "ureq",
            "surf",
            "isahc",
            "tokio",
            "tokio-util",
            "tokio-tungstenite",
            "tungstenite",
            "subprocess",
            "duct",
            "xshell",
            "clap",
            "tracing",
            "tracing-subscriber",
        ],
        "deja-store" => &[
            "reqwest",
            "hyper",
            "tokio-tungstenite",
            "tungstenite",
            "clap",
            "tracing-subscriber",
        ],
        "deja-flow" => &[
            "rusqlite",
            "libsqlite3-sys",
            "sqlite-wasm-rs",
            "reqwest",
            "hyper",
            "tokio-tungstenite",
            "tungstenite",
            "clap",
            "tracing-subscriber",
        ],
        "deja-driver-ios" | "deja-driver-android" => &[
            "rusqlite",
            "libsqlite3-sys",
            "sqlite-wasm-rs",
            "tokio-tungstenite",
            "tungstenite",
            "clap",
            "tracing-subscriber",
        ],
        "deja-driver-web" => &[
            "rusqlite",
            "libsqlite3-sys",
            "sqlite-wasm-rs",
            "clap",
            "tracing-subscriber",
        ],
        "deja-explore" | "deja-mcp" => &[
            "tokio-tungstenite",
            "tungstenite",
            "clap",
            "tracing-subscriber",
        ],
        _ => &[],
    }
}

fn forbidden_external_direct_only(crate_name: &str) -> &'static [&'static str] {
    match crate_name {
        "deja-mcp" => &["reqwest", "hyper"],
        _ => &[],
    }
}

fn validate_workspace_boundaries(snapshot: &WorkspaceSnapshot) -> Result<(), Vec<String>> {
    let mut violations = Vec::new();
    let expected: BTreeSet<&str> = EXPECTED_CRATES.iter().copied().collect();
    let actual: BTreeSet<&str> = snapshot.crates.keys().map(String::as_str).collect();

    for missing in expected.difference(&actual) {
        violations.push(format!("missing expected workspace crate: {missing}"));
    }
    for extra in actual.difference(&expected) {
        violations.push(format!("undocumented workspace crate: {extra}"));
    }

    for (crate_name, deps) in &snapshot.crates {
        let Some(allowed_internal) = allowed_internal_deps(crate_name) else {
            continue;
        };

        let mut internal_checks: Vec<(&str, &str)> = Vec::new();
        for dep in &deps.direct_normal_and_build {
            if actual.contains(dep.as_str()) {
                internal_checks.push(("direct", dep));
            }
        }
        for dep in &deps.direct_dev {
            if actual.contains(dep.as_str()) {
                internal_checks.push(("dev", dep));
            }
        }
        for dep in &deps.transitive_normal_and_build {
            if actual.contains(dep.as_str()) {
                internal_checks.push(("transitive", dep));
            }
        }
        for dep in &deps.transitive_dev {
            if actual.contains(dep.as_str()) {
                internal_checks.push(("transitive-dev", dep));
            }
        }

        for (edge_kind, target) in internal_checks {
            if target == crate_name {
                violations.push(format!(
                    "{crate_name} has forbidden self-dependency ({edge_kind})"
                ));
                continue;
            }
            if target == "deja-cli" {
                violations.push(format!(
                    "{crate_name} -> deja-cli ({edge_kind}) violates composition-root rule: no crate may depend on deja-cli"
                ));
                continue;
            }
            if !allowed_internal.contains(&target) {
                let detail = if DRIVER_CRATES.contains(&crate_name.as_str())
                    && DRIVER_CRATES.contains(&target)
                {
                    "cross-driver edge is forbidden"
                } else if DRIVER_CRATES.contains(&target) || target == "deja-mcp" {
                    "only deja-cli may depend on concrete drivers or deja-mcp"
                } else {
                    "edge is not allowed in workspace DAG"
                };
                violations.push(format!(
                    "{crate_name} -> {target} ({edge_kind}) forbidden: {detail}"
                ));
            }
        }

        if crate_name == "deja-core" {
            for dep in &deps.direct_normal_and_build {
                if !actual.contains(dep.as_str())
                    && !ALLOWED_CORE_DIRECT_DEPS.contains(&dep.as_str())
                {
                    violations.push(format!(
                        "deja-core directly depends on unapproved external crate `{dep}` (not in core allowlist)"
                    ));
                }
            }
            for dep in &deps.direct_dev {
                if !actual.contains(dep.as_str())
                    && !ALLOWED_CORE_DIRECT_DEV_DEPS.contains(&dep.as_str())
                {
                    violations.push(format!(
                        "deja-core has unapproved external dev-dependency `{dep}` (not in core dev allowlist)"
                    ));
                }
            }
        }

        for forbidden in GLOBAL_FORBIDDEN_EXTERNAL {
            if deps.direct_normal_and_build.contains(*forbidden)
                || deps.direct_dev.contains(*forbidden)
                || deps.transitive_normal_and_build.contains(*forbidden)
                || deps.transitive_dev.contains(*forbidden)
            {
                violations.push(format!(
                    "{crate_name} pulls globally forbidden TLS/OpenSSL crate `{forbidden}`"
                ));
            }
        }

        for (owner, owned_crates) in DIRECT_OWNER_RULES {
            if crate_name == owner {
                continue;
            }
            for owned in *owned_crates {
                if deps.direct_normal_and_build.contains(*owned) || deps.direct_dev.contains(*owned)
                {
                    violations.push(format!(
                        "{crate_name} directly depends on `{owned}`, which is restricted to `{owner}`"
                    ));
                }
            }
        }

        for forbidden in forbidden_external_all(crate_name) {
            if deps.direct_normal_and_build.contains(*forbidden) {
                violations.push(format!(
                    "{crate_name} directly depends on forbidden external crate `{forbidden}`"
                ));
            }
            if deps.direct_dev.contains(*forbidden) {
                violations.push(format!(
                    "{crate_name} has forbidden external dev-dependency `{forbidden}`"
                ));
            }
            if deps.transitive_normal_and_build.contains(*forbidden) {
                violations.push(format!(
                    "{crate_name} transitively depends on forbidden external crate `{forbidden}`"
                ));
            }
            if deps.transitive_dev.contains(*forbidden) {
                violations.push(format!(
                    "{crate_name} transitively dev-depends on forbidden external crate `{forbidden}`"
                ));
            }
        }

        for forbidden in forbidden_external_direct_only(crate_name) {
            if deps.direct_normal_and_build.contains(*forbidden)
                || deps.direct_dev.contains(*forbidden)
            {
                violations.push(format!(
                    "{crate_name} directly depends on forbidden external crate `{forbidden}`"
                ));
            }
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("rust workspace root should exist")
}

fn run_cargo_metadata(args: &[&str]) -> CargoMetadata {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(&cargo)
        .current_dir(workspace_root())
        .arg("metadata")
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run `{cargo} metadata {args:?}`: {err}"));

    assert!(
        output.status.success(),
        "`cargo metadata {args:?}` failed with {}:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("failed to parse `cargo metadata {args:?}` JSON: {err}"))
}

fn host_target_triple() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(&rustc)
        .arg("-vV")
        .output()
        .unwrap_or_else(|err| panic!("failed to run `{rustc} -vV`: {err}"));

    assert!(
        output.status.success(),
        "`{rustc} -vV` failed with {}:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::trim))
        .expect("`rustc -vV` output should include a `host:` line")
        .to_string()
}

fn check_all_targets_enabled() -> bool {
    std::env::var("DEJA_BOUNDARIES_ALL_TARGETS")
        .map(|val| val == "1" || val.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || std::env::var("CI")
            .map(|val| val == "1" || val.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

fn has_non_dev_edge(dep: &ResolveDep) -> bool {
    dep.dep_kinds.is_empty()
        || dep
            .dep_kinds
            .iter()
            .any(|info| info.kind.as_deref() != Some("dev"))
}

fn has_dev_edge(dep: &ResolveDep) -> bool {
    dep.dep_kinds
        .iter()
        .any(|info| info.kind.as_deref() == Some("dev"))
}

fn collect_non_dev_closure<'a>(
    roots: impl IntoIterator<Item = &'a str>,
    skip_id: &'a str,
    node_by_id: &HashMap<&'a str, &'a ResolveNode>,
    id_to_name: &HashMap<&'a str, &'a str>,
) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    visited.insert(skip_id);

    for root in roots {
        if visited.insert(root) {
            if let Some(name) = id_to_name.get(root) {
                result.insert((*name).to_string());
            }
            queue.push_back(root);
        }
    }

    while let Some(current_id) = queue.pop_front() {
        let Some(node) = node_by_id.get(current_id) else {
            continue;
        };
        for dep in &node.deps {
            if !has_non_dev_edge(dep) {
                continue;
            }
            if visited.insert(dep.pkg.as_str()) {
                if let Some(dep_name) = id_to_name.get(dep.pkg.as_str()) {
                    result.insert((*dep_name).to_string());
                }
                queue.push_back(dep.pkg.as_str());
            }
        }
    }

    result
}

fn load_live_workspace_snapshot() -> WorkspaceSnapshot {
    let no_deps = run_cargo_metadata(&["--no-deps", "--format-version", "1"]);
    let resolved = if check_all_targets_enabled() {
        run_cargo_metadata(&[
            "--format-version",
            "1",
            "--all-features",
            "--locked",
            "--offline",
        ])
    } else {
        let host = host_target_triple();
        run_cargo_metadata(&[
            "--format-version",
            "1",
            "--all-features",
            "--locked",
            "--offline",
            "--filter-platform",
            &host,
        ])
    };

    let member_ids: BTreeSet<&str> = no_deps
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();

    let id_to_name: HashMap<&str, &str> = resolved
        .packages
        .iter()
        .map(|pkg| (pkg.id.as_str(), pkg.name.as_str()))
        .collect();

    let resolve = resolved
        .resolve
        .as_ref()
        .expect("resolved cargo metadata must include dependency graph");
    let node_by_id: HashMap<&str, &ResolveNode> = resolve
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();

    let mut crates = BTreeMap::new();
    for pkg in &no_deps.packages {
        if !member_ids.contains(pkg.id.as_str()) {
            continue;
        }

        let mut direct_normal_and_build = BTreeSet::new();
        let mut direct_dev = BTreeSet::new();
        for dep in &pkg.dependencies {
            if dep.kind.as_deref() == Some("dev") {
                direct_dev.insert(dep.name.clone());
            } else {
                direct_normal_and_build.insert(dep.name.clone());
            }
        }

        let (direct_non_dev_ids, direct_dev_ids): (Vec<&str>, Vec<&str>) = node_by_id
            .get(pkg.id.as_str())
            .map(|node| {
                let non_dev = node
                    .deps
                    .iter()
                    .filter(|dep| has_non_dev_edge(dep))
                    .map(|dep| dep.pkg.as_str())
                    .collect();
                let dev = node
                    .deps
                    .iter()
                    .filter(|dep| has_dev_edge(dep))
                    .map(|dep| dep.pkg.as_str())
                    .collect();
                (non_dev, dev)
            })
            .unwrap_or_default();

        let transitive_normal_and_build = collect_non_dev_closure(
            direct_non_dev_ids,
            pkg.id.as_str(),
            &node_by_id,
            &id_to_name,
        );
        let transitive_dev =
            collect_non_dev_closure(direct_dev_ids, pkg.id.as_str(), &node_by_id, &id_to_name);

        crates.insert(
            pkg.name.clone(),
            CrateDeps {
                direct_normal_and_build,
                direct_dev,
                transitive_normal_and_build,
                transitive_dev,
            },
        );
    }

    WorkspaceSnapshot { crates }
}

fn set_of(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

fn valid_baseline_snapshot() -> WorkspaceSnapshot {
    let mut crates = BTreeMap::new();
    let entries: &[(&str, &[&str], &[&str])] = &[
        (
            "deja-core",
            &["saphyr", "serde", "sha2"],
            &["saphyr", "serde", "sha2"],
        ),
        (
            "deja-store",
            &["deja-core", "rusqlite", "tracing"],
            &["deja-core", "rusqlite", "libsqlite3-sys", "tracing"],
        ),
        (
            "deja-flow",
            &["deja-core", "tokio", "tracing"],
            &["deja-core", "tokio", "tracing"],
        ),
        (
            "deja-driver-ios",
            &["deja-core", "deja-flow", "reqwest", "tokio"],
            &["deja-core", "deja-flow", "reqwest", "hyper", "tokio"],
        ),
        (
            "deja-driver-android",
            &["deja-core", "deja-flow", "reqwest", "tokio"],
            &["deja-core", "deja-flow", "reqwest", "hyper", "tokio"],
        ),
        (
            "deja-driver-web",
            &["deja-core", "deja-flow", "tokio", "tokio-tungstenite"],
            &[
                "deja-core",
                "deja-flow",
                "tokio",
                "tokio-tungstenite",
                "tungstenite",
            ],
        ),
        (
            "deja-explore",
            &["deja-core", "deja-store", "deja-flow", "reqwest", "tokio"],
            &[
                "deja-core",
                "deja-store",
                "deja-flow",
                "rusqlite",
                "reqwest",
                "tokio",
            ],
        ),
        (
            "deja-mcp",
            &[
                "deja-core",
                "deja-store",
                "deja-flow",
                "deja-explore",
                "tokio",
            ],
            &[
                "deja-core",
                "deja-store",
                "deja-flow",
                "deja-explore",
                "rusqlite",
                "reqwest",
                "tokio",
            ],
        ),
        (
            "deja-cli",
            &[
                "deja-core",
                "deja-store",
                "deja-flow",
                "deja-driver-ios",
                "deja-driver-android",
                "deja-driver-web",
                "deja-explore",
                "deja-mcp",
                "clap",
                "tracing-subscriber",
            ],
            &[
                "deja-core",
                "deja-store",
                "deja-flow",
                "deja-driver-ios",
                "deja-driver-android",
                "deja-driver-web",
                "deja-explore",
                "deja-mcp",
                "clap",
                "rusqlite",
                "reqwest",
                "tokio-tungstenite",
                "tracing-subscriber",
            ],
        ),
    ];

    for (name, direct, transitive) in entries {
        crates.insert(
            (*name).to_string(),
            CrateDeps {
                direct_normal_and_build: set_of(direct),
                direct_dev: BTreeSet::new(),
                transitive_normal_and_build: set_of(transitive),
                transitive_dev: BTreeSet::new(),
            },
        );
    }

    WorkspaceSnapshot { crates }
}

fn assert_rejected(snapshot: &WorkspaceSnapshot, expected_substring: &str) {
    let err = validate_workspace_boundaries(snapshot)
        .expect_err("expected workspace boundary validation to fail");
    assert!(
        err.iter().any(|msg| msg.contains(expected_substring)),
        "expected error mentioning `{expected_substring}`, got: {err:#?}"
    );
}

#[test]
fn live_workspace_satisfies_all_boundaries() {
    let snapshot = load_live_workspace_snapshot();
    if let Err(violations) = validate_workspace_boundaries(&snapshot) {
        panic!(
            "workspace boundary violations:\n- {}",
            violations.join("\n- ")
        );
    }
}

#[test]
fn baseline_snapshot_is_valid() {
    let snapshot = valid_baseline_snapshot();
    assert!(validate_workspace_boundaries(&snapshot).is_ok());
}

#[test]
fn rejects_core_depending_on_store() {
    let mut snapshot = valid_baseline_snapshot();
    snapshot
        .crates
        .get_mut("deja-core")
        .unwrap()
        .direct_normal_and_build
        .insert("deja-store".to_string());
    assert_rejected(&snapshot, "deja-core -> deja-store");
}

#[test]
fn rejects_core_depending_on_flow() {
    let mut snapshot = valid_baseline_snapshot();
    snapshot
        .crates
        .get_mut("deja-core")
        .unwrap()
        .direct_normal_and_build
        .insert("deja-flow".to_string());
    assert_rejected(&snapshot, "deja-core -> deja-flow");
}

#[test]
fn rejects_cross_driver_edges() {
    for (from, to) in [
        ("deja-driver-ios", "deja-driver-android"),
        ("deja-driver-android", "deja-driver-web"),
        ("deja-driver-web", "deja-driver-ios"),
    ] {
        let mut snapshot = valid_baseline_snapshot();
        snapshot
            .crates
            .get_mut(from)
            .unwrap()
            .direct_normal_and_build
            .insert(to.to_string());
        assert_rejected(&snapshot, "cross-driver edge is forbidden");
    }
}

#[test]
fn rejects_core_direct_dev_and_transitive_rusqlite() {
    let mut direct = valid_baseline_snapshot();
    direct
        .crates
        .get_mut("deja-core")
        .unwrap()
        .direct_normal_and_build
        .insert("rusqlite".to_string());
    assert_rejected(&direct, "rusqlite");

    let mut dev = valid_baseline_snapshot();
    dev.crates
        .get_mut("deja-core")
        .unwrap()
        .direct_dev
        .insert("rusqlite".to_string());
    assert_rejected(&dev, "rusqlite");

    let mut transitive = valid_baseline_snapshot();
    transitive
        .crates
        .get_mut("deja-core")
        .unwrap()
        .transitive_normal_and_build
        .insert("rusqlite".to_string());
    assert_rejected(
        &transitive,
        "transitively depends on forbidden external crate `rusqlite`",
    );
}

#[test]
fn rejects_core_reqwest_and_tokio() {
    for forbidden in ["reqwest", "tokio"] {
        let mut direct = valid_baseline_snapshot();
        direct
            .crates
            .get_mut("deja-core")
            .unwrap()
            .direct_normal_and_build
            .insert(forbidden.to_string());
        assert_rejected(&direct, forbidden);

        let mut transitive = valid_baseline_snapshot();
        transitive
            .crates
            .get_mut("deja-core")
            .unwrap()
            .transitive_normal_and_build
            .insert(forbidden.to_string());
        assert_rejected(&transitive, forbidden);
    }
}

#[test]
fn rejects_core_unapproved_process_or_http_crates() {
    for unapproved in ["subprocess", "duct", "ureq"] {
        let mut direct = valid_baseline_snapshot();
        direct
            .crates
            .get_mut("deja-core")
            .unwrap()
            .direct_normal_and_build
            .insert(unapproved.to_string());
        assert_rejected(&direct, "not in core allowlist");

        let mut dev = valid_baseline_snapshot();
        dev.crates
            .get_mut("deja-core")
            .unwrap()
            .direct_dev
            .insert(unapproved.to_string());
        assert_rejected(&dev, "not in core dev allowlist");

        let mut transitive = valid_baseline_snapshot();
        transitive
            .crates
            .get_mut("deja-core")
            .unwrap()
            .transitive_normal_and_build
            .insert(unapproved.to_string());
        assert_rejected(
            &transitive,
            &format!("transitively depends on forbidden external crate `{unapproved}`"),
        );

        let mut transitive_dev = valid_baseline_snapshot();
        transitive_dev
            .crates
            .get_mut("deja-core")
            .unwrap()
            .transitive_dev
            .insert(unapproved.to_string());
        assert_rejected(
            &transitive_dev,
            &format!("transitively dev-depends on forbidden external crate `{unapproved}`"),
        );
    }
}

#[test]
fn rejects_transitive_dev_dep_pulling_forbidden_tls() {
    let mut snapshot = valid_baseline_snapshot();
    snapshot
        .crates
        .get_mut("deja-cli")
        .unwrap()
        .direct_dev
        .insert("mock-http-helper".to_string());
    snapshot
        .crates
        .get_mut("deja-cli")
        .unwrap()
        .transitive_dev
        .insert("native-tls".to_string());
    assert_rejected(&snapshot, "native-tls");

    let mut core_dev = valid_baseline_snapshot();
    core_dev
        .crates
        .get_mut("deja-core")
        .unwrap()
        .transitive_dev
        .insert("reqwest".to_string());
    assert_rejected(&core_dev, "reqwest");
}

#[test]
fn rejects_explore_depending_on_concrete_drivers() {
    for driver in DRIVER_CRATES {
        let mut snapshot = valid_baseline_snapshot();
        snapshot
            .crates
            .get_mut("deja-explore")
            .unwrap()
            .direct_normal_and_build
            .insert((*driver).to_string());
        assert_rejected(&snapshot, &format!("deja-explore -> {driver}"));
    }
}

#[test]
fn rejects_mcp_depending_on_concrete_drivers_or_cli() {
    for target in ["deja-driver-web", "deja-driver-ios", "deja-cli"] {
        let mut snapshot = valid_baseline_snapshot();
        snapshot
            .crates
            .get_mut("deja-mcp")
            .unwrap()
            .direct_normal_and_build
            .insert(target.to_string());
        assert_rejected(&snapshot, &format!("deja-mcp -> {target}"));
    }
}

#[test]
fn rejects_non_cli_depending_on_cli() {
    let mut snapshot = valid_baseline_snapshot();
    snapshot
        .crates
        .get_mut("deja-flow")
        .unwrap()
        .direct_normal_and_build
        .insert("deja-cli".to_string());
    assert_rejected(&snapshot, "deja-flow -> deja-cli");
}

#[test]
fn rejects_invalid_store_flow_lateral_edges() {
    let mut store_to_flow = valid_baseline_snapshot();
    store_to_flow
        .crates
        .get_mut("deja-store")
        .unwrap()
        .direct_normal_and_build
        .insert("deja-flow".to_string());
    assert_rejected(&store_to_flow, "deja-store -> deja-flow");

    let mut flow_to_store = valid_baseline_snapshot();
    flow_to_store
        .crates
        .get_mut("deja-flow")
        .unwrap()
        .direct_normal_and_build
        .insert("deja-store".to_string());
    assert_rejected(&flow_to_store, "deja-flow -> deja-store");
}

#[test]
fn rejects_forbidden_external_deps_in_other_crates() {
    let cases = [
        ("deja-explore", "clap", false),
        ("deja-explore", "rusqlite", false),
        ("deja-mcp", "reqwest", false),
        ("deja-driver-ios", "tokio-tungstenite", false),
        ("deja-cli", "openssl-sys", true),
    ];

    for (crate_name, dep, transitive_only) in cases {
        let mut snapshot = valid_baseline_snapshot();
        let entry = snapshot.crates.get_mut(crate_name).unwrap();
        if transitive_only {
            entry.transitive_normal_and_build.insert(dep.to_string());
        } else {
            entry.direct_normal_and_build.insert(dep.to_string());
        }
        assert_rejected(&snapshot, dep);
    }
}

#[test]
fn rejects_undocumented_workspace_crate() {
    let mut snapshot = valid_baseline_snapshot();
    snapshot.crates.insert(
        "deja-rogue".to_string(),
        CrateDeps {
            direct_normal_and_build: BTreeSet::new(),
            direct_dev: BTreeSet::new(),
            transitive_normal_and_build: BTreeSet::new(),
            transitive_dev: BTreeSet::new(),
        },
    );
    assert_rejected(&snapshot, "undocumented workspace crate: deja-rogue");
}
