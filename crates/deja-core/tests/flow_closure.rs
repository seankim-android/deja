//! Tests for `flow_references`, `flow_closure` confinement/limits/media/stored checks,
//! and `DEJA-38` L2 closure assertion memoization and line cap.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use deja_core::{
    CoreError, FlowReference, FlowReferenceCommand, FlowReferenceKind, MAX_CLOSURE_ASSERTION_BYTES,
    MAX_CLOSURE_ASSERTIONS, MAX_DEPTH, MAX_FILE_BYTES, MAX_FILES, MAX_MEDIA_BYTES,
    MAX_TOTAL_MEDIA_BYTES, StoredClosureCheck, StoredFlowFile, check_stored_subflows,
    closure_assertions, collect_flow_closure, find_flow_references, plan_stored_closure,
    read_stored_flow_files, spelling_key,
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
            "deja-closure-test-{prefix}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        let canonical = fs::canonicalize(&dir).expect("canonicalize temp dir");
        Self { path: canonical }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let target = self.path.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&target, bytes).expect("write fixture file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// =============================================================================================
// packages/core/test/flow-references.test.ts (all 6 test blocks)

#[test]
fn find_flow_references_string_and_file_forms() {
    let yaml = [
        "- runFlow: a.yaml",
        "- runFlow:",
        "    file: ../b.yaml",
        "    env: { X: 1 }",
        "- runScript: c.js",
        "- runScript:",
        "    file: d.js",
        "",
    ]
    .join("\n");
    assert_eq!(
        find_flow_references(&yaml).expect("valid"),
        vec![
            FlowReference {
                kind: FlowReferenceKind::Flow,
                command: FlowReferenceCommand::RunFlow,
                path: "a.yaml".into(),
            },
            FlowReference {
                kind: FlowReferenceKind::Flow,
                command: FlowReferenceCommand::RunFlow,
                path: "../b.yaml".into(),
            },
            FlowReference {
                kind: FlowReferenceKind::Script,
                command: FlowReferenceCommand::RunScript,
                path: "c.js".into(),
            },
            FlowReference {
                kind: FlowReferenceKind::Script,
                command: FlowReferenceCommand::RunScript,
                path: "d.js".into(),
            },
        ]
    );
}

#[test]
fn find_flow_references_add_media_and_retry() {
    let yaml = [
        "- addMedia:",
        "    - one.png",
        "    - two.png",
        "    - ../clips/three.mp4",
        "- addMedia:",
        "    files: [four.jpg]",
        "- retry:",
        "    maxRetries: 2",
        "    file: again.yaml",
        "- retry:",
        "    commands:",
        "      - addMedia: [nested.gif]",
        "",
    ]
    .join("\n");
    assert_eq!(
        find_flow_references(&yaml).expect("valid"),
        vec![
            FlowReference {
                kind: FlowReferenceKind::Media,
                command: FlowReferenceCommand::AddMedia,
                path: "one.png".into(),
            },
            FlowReference {
                kind: FlowReferenceKind::Media,
                command: FlowReferenceCommand::AddMedia,
                path: "two.png".into(),
            },
            FlowReference {
                kind: FlowReferenceKind::Media,
                command: FlowReferenceCommand::AddMedia,
                path: "../clips/three.mp4".into(),
            },
            FlowReference {
                kind: FlowReferenceKind::Media,
                command: FlowReferenceCommand::AddMedia,
                path: "four.jpg".into(),
            },
            FlowReference {
                kind: FlowReferenceKind::Flow,
                command: FlowReferenceCommand::Retry,
                path: "again.yaml".into(),
            },
            FlowReference {
                kind: FlowReferenceKind::Media,
                command: FlowReferenceCommand::AddMedia,
                path: "nested.gif".into(),
            },
        ]
    );
}

#[test]
fn find_flow_references_refuses_malformed_and_unsupported_commands() {
    assert!(matches!(
        find_flow_references("- addMedia:\n"),
        Err(CoreError::FlowFileCommand(_))
    ));
    assert!(matches!(
        find_flow_references("- addMedia: []\n"),
        Err(CoreError::FlowFileCommand(_))
    ));
    assert!(matches!(
        find_flow_references("- addMedia:\n    - 3\n"),
        Err(CoreError::FlowFileCommand(_))
    ));
    assert!(matches!(
        find_flow_references("- addMedia: one.png\n"),
        Err(CoreError::FlowFileCommand(_))
    ));
    assert!(matches!(
        find_flow_references("- addMedia:\n    file: one.png\n"),
        Err(CoreError::FlowFileCommand(_))
    ));
    assert!(matches!(
        find_flow_references("- retry: again.yaml\n"),
        Err(CoreError::FlowFileCommand(_))
    ));
    let screenshot_err = find_flow_references("- assertScreenshot: home.png\n").unwrap_err();
    assert!(matches!(screenshot_err, CoreError::FlowFileCommand(_)));
    assert!(screenshot_err.to_string().contains("reference image"));
    assert!(
        find_flow_references("- repeat:\n    commands:\n      - assertScreenshot: home\n").is_err()
    );
}

#[test]
fn find_flow_references_all_documents_hooks_quoted_and_broken_inputs() {
    let nested = [
        "appId: app",
        "onFlowStart:",
        "  - runScript: start.js",
        "onFlowComplete:",
        "  - runFlow: done.yaml",
        "---",
        "- repeat:",
        "    times: 2",
        "    commands:",
        "      - retry:",
        "          commands:",
        "            - runFlow: deep.yaml",
        "- runFlow:",
        "    when:",
        "      visible: Ok",
        "    commands:",
        "      - runFlow: inline.yaml",
        "",
    ]
    .join("\n");
    let paths: Vec<String> = find_flow_references(&nested)
        .expect("valid")
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert_eq!(
        paths,
        vec!["start.js", "done.yaml", "deep.yaml", "inline.yaml"]
    );

    let quoted = "- \"runFlow\": a.yaml\n- { runScript: { file: \"b.js\" } }\n- {\"runFlow\": {\"file\": \"c.yaml\"}}\n";
    let quoted_paths: Vec<String> = find_flow_references(quoted)
        .expect("valid")
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert_eq!(quoted_paths, vec!["a.yaml", "b.js", "c.yaml"]);

    assert_eq!(
        find_flow_references("appId: app\n---\n- tapOn: Save\n").expect("valid"),
        vec![]
    );
    assert!(find_flow_references("- runFlow:\n").is_err());
    assert!(find_flow_references("- runScript:\n    env: {}\n").is_err());
    assert!(find_flow_references("appId: app\n- assertVisible: Done\n").is_err());
    assert!(find_flow_references("- runFlow: [unclosed").is_err());
}

// =============================================================================================
// Confinement, symlinks, limits, media, spelling_key, stored closure checks

#[test]
fn closure_keeps_media_bytes_and_enforces_size_caps() {
    assert_eq!(MAX_FILES, 64);
    assert_eq!(MAX_DEPTH, 16);
    assert_eq!(MAX_FILE_BYTES, 1024 * 1024);
    assert_eq!(MAX_MEDIA_BYTES, 8 * 1024 * 1024);
    assert_eq!(MAX_TOTAL_MEDIA_BYTES, 32 * 1024 * 1024);

    let picture = vec![0x89, 0x50, 0xff, 0xfe, 0x00, 0x81, 0xc3, 0x28];
    let tmp = TempDir::new("media-bytes");
    tmp.write(
        ".maestro/flows/main.yaml",
        "- addMedia:\n    - ../media/pic.png\n",
    );
    tmp.write(".maestro/media/pic.png", &picture);

    let closure = collect_flow_closure(tmp.path(), &tmp.path().join(".maestro/flows/main.yaml"))
        .expect("closure succeeds");
    let media = closure
        .files
        .iter()
        .find(|f| f.kind == FlowReferenceKind::Media)
        .expect("media file");
    assert_eq!(media.path, "media/pic.png");
    assert_eq!(media.source, ".maestro/media/pic.png");
    assert_eq!(media.bytes, picture);

    // Single media over 8 MiB fails closed.
    let big_tmp = TempDir::new("media-big");
    big_tmp.write(
        ".maestro/flows/main.yaml",
        "- addMedia: [../media/big.mp4]\n",
    );
    big_tmp.write(
        ".maestro/media/big.mp4",
        vec![0u8; (MAX_MEDIA_BYTES as usize) + 1],
    );
    let err = collect_flow_closure(
        big_tmp.path(),
        &big_tmp.path().join(".maestro/flows/main.yaml"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("under 8 MiB"));

    // Total media over 32 MiB fails closed even when each file is <= 8 MiB.
    let total_tmp = TempDir::new("media-total");
    total_tmp.write(
        ".maestro/flows/main.yaml",
        "- addMedia: [../media/1.mp4, ../media/2.mp4, ../media/3.mp4, ../media/4.mp4, ../media/5.mp4]\n",
    );
    for n in 1..=5u8 {
        total_tmp.write(&format!(".maestro/media/{n}.mp4"), vec![n; 7 * 1024 * 1024]);
    }
    let total_err = collect_flow_closure(
        total_tmp.path(),
        &total_tmp.path().join(".maestro/flows/main.yaml"),
    )
    .unwrap_err();
    assert!(total_err.to_string().contains("add up to more than 32 MiB"));

    // Single flow over 1 MiB fails closed.
    let flow_big = TempDir::new("flow-big");
    flow_big.write(".maestro/flows/main.yaml", "- runScript: big.js\n");
    flow_big.write(
        ".maestro/flows/big.js",
        vec![b'a'; (MAX_FILE_BYTES as usize) + 1],
    );
    let flow_big_err = collect_flow_closure(
        flow_big.path(),
        &flow_big.path().join(".maestro/flows/main.yaml"),
    )
    .unwrap_err();
    assert!(flow_big_err.to_string().contains("under 1 MiB"));
}

#[test]
fn closure_rejects_nfc_and_case_spelling_collisions_and_kind_conflicts() {
    assert_eq!(
        spelling_key("caf\u{00e9}.png"),
        spelling_key("cafe\u{0301}.png")
    );
    assert_eq!(
        spelling_key("Flows/Today.yaml"),
        spelling_key("flows/today.yaml")
    );

    // NFC vs NFD spelling collision.
    let tmp = TempDir::new("nfc-collision");
    tmp.write(
        ".maestro/flows/main.yaml",
        "- addMedia: [../media/caf\u{00e9}.png]\n- addMedia: [../media/cafe\u{0301}.png]\n",
    );
    tmp.write(".maestro/media/caf\u{00e9}.png", "x");
    let err =
        collect_flow_closure(tmp.path(), &tmp.path().join(".maestro/flows/main.yaml")).unwrap_err();
    assert!(matches!(err, CoreError::FlowClosure(_)));
    assert!(err.to_string().contains("Unicode normalization"));

    // One file used as media and as flow.
    let kind_tmp = TempDir::new("kind-conflict");
    kind_tmp.write(
        ".maestro/flows/main.yaml",
        "- addMedia: [x.yaml]\n- runFlow: x.yaml\n",
    );
    kind_tmp.write(".maestro/flows/x.yaml", "- runFlow: ../nowhere.yaml\n");
    let kind_err = collect_flow_closure(
        kind_tmp.path(),
        &kind_tmp.path().join(".maestro/flows/main.yaml"),
    )
    .unwrap_err();
    assert!(
        kind_err
            .to_string()
            .contains("used as a media and as a flow")
    );
}

#[test]
fn closure_enforces_workspace_confinement_symlinks_secrets_cycles_and_limits() {
    let outer = TempDir::new("confinement-outer");
    let ws = outer.path().join("workspace");
    fs::create_dir_all(ws.join("flows")).expect("mkdir workspace");
    fs::write(outer.path().join("outside.yaml"), "- tapOn: x\n").expect("write outside");

    // Lexical escape rejected.
    fs::write(
        ws.join("flows/escape.yaml"),
        "- runFlow: ../../outside.yaml\n",
    )
    .expect("write");
    assert!(
        collect_flow_closure(&ws, &ws.join("flows/escape.yaml"))
            .unwrap_err()
            .to_string()
            .contains("escapes the workspace")
    );

    // Symlink escape rejected.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            outer.path().join("outside.yaml"),
            ws.join("flows/link.yaml"),
        )
        .expect("symlink");
        fs::write(ws.join("flows/via-link.yaml"), "- runFlow: link.yaml\n").expect("write");
        assert!(
            collect_flow_closure(&ws, &ws.join("flows/via-link.yaml"))
                .unwrap_err()
                .to_string()
                .contains("escapes the workspace")
        );
    }

    // Secret/hidden file rejected.
    fs::write(ws.join("flows/.env"), "SECRET=1\n").expect("write .env");
    fs::write(ws.join("flows/read-env.yaml"), "- runScript: .env\n").expect("write");
    assert!(
        collect_flow_closure(&ws, &ws.join("flows/read-env.yaml"))
            .unwrap_err()
            .to_string()
            .contains("secret or hidden")
    );

    // Cycle rejected.
    fs::write(ws.join("flows/a.yaml"), "- runFlow: b.yaml\n").expect("write a");
    fs::write(ws.join("flows/b.yaml"), "- runFlow: a.yaml\n").expect("write b");
    assert!(
        collect_flow_closure(&ws, &ws.join("flows/a.yaml"))
            .unwrap_err()
            .to_string()
            .contains("cycle")
    );

    // Variable reference rejected.
    fs::write(ws.join("flows/var.yaml"), "- runFlow: ${SUB}.yaml\n").expect("write var");
    assert!(
        collect_flow_closure(&ws, &ws.join("flows/var.yaml"))
            .unwrap_err()
            .to_string()
            .contains("variable")
    );

    // Missing file returns MissingFlowFile.
    fs::write(ws.join("flows/missing-ref.yaml"), "- runFlow: nope.yaml\n").expect("write");
    assert!(matches!(
        collect_flow_closure(&ws, &ws.join("flows/missing-ref.yaml")),
        Err(CoreError::MissingFlowFile { path }) if path == "flows/nope.yaml"
    ));

    // MAX_DEPTH exceeded (> 16).
    let depth_tmp = TempDir::new("max-depth");
    for i in 0..=MAX_DEPTH {
        let content = if i < MAX_DEPTH {
            format!("- runFlow: f{}.yaml\n", i + 1)
        } else {
            "- tapOn: leaf\n".to_string()
        };
        depth_tmp.write(&format!("flows/f{i}.yaml"), content);
    }
    assert!(
        collect_flow_closure(depth_tmp.path(), &depth_tmp.path().join("flows/f0.yaml"))
            .unwrap_err()
            .to_string()
            .contains("nested more than 16 deep")
    );

    // MAX_FILES exceeded (> 64).
    let files_tmp = TempDir::new("max-files");
    let mut main_yaml = String::new();
    for i in 0..MAX_FILES {
        main_yaml.push_str(&format!("- runFlow: sub{i}.yaml\n"));
        files_tmp.write(&format!("flows/sub{i}.yaml"), "- tapOn: x\n");
    }
    files_tmp.write("flows/main.yaml", main_yaml);
    assert!(
        collect_flow_closure(files_tmp.path(), &files_tmp.path().join("flows/main.yaml"))
            .unwrap_err()
            .to_string()
            .contains("more than 64 files")
    );
}

#[test]
fn plan_and_check_stored_subflows_match_typescript() {
    let tmp = TempDir::new("stored-subflows");
    tmp.write(
        ".deja/flows/g/android-aa/main.yaml",
        "- addMedia: [pic.png]\n",
    );
    tmp.write(".deja/flows/g/android-aa/pic.png", "x");
    tmp.write(
        ".deja/flows/g/android-dd/main.yaml",
        "- assertScreenshot: home.png\n",
    );
    tmp.write(
        ".deja/flows/g/android-bb/main.yaml",
        "- retry:\n    file: again.yaml\n",
    );
    tmp.write(".deja/flows/g/android-bb/again.yaml", "- tapOn: x\n");
    tmp.write(
        ".deja/flows/g/android-cc/main.yaml",
        "- runFlow: again.yaml\n",
    );
    tmp.write(".deja/flows/g/android-cc/again.yaml", "- tapOn: x\n");

    assert_eq!(
        check_stored_subflows(
            tmp.path(),
            ".deja/flows/g/android-aa/main.yaml",
            Some(&[]),
            false
        )
        .expect("check"),
        StoredClosureCheck::Legacy
    );
    assert_eq!(
        check_stored_subflows(
            tmp.path(),
            ".deja/flows/g/android-bb/main.yaml",
            Some(&[]),
            false
        )
        .expect("check"),
        StoredClosureCheck::Legacy
    );
    assert_eq!(
        check_stored_subflows(
            tmp.path(),
            ".deja/flows/g/android-dd/main.yaml",
            Some(&[]),
            false
        )
        .expect("check"),
        StoredClosureCheck::Legacy
    );
    assert_eq!(
        check_stored_subflows(
            tmp.path(),
            ".deja/flows/g/android-dd/main.yaml",
            None,
            false
        )
        .expect("check"),
        StoredClosureCheck::Legacy
    );
    assert!(matches!(
        check_stored_subflows(
            tmp.path(),
            ".deja/flows/g/android-cc/main.yaml",
            Some(&[]),
            false
        )
        .expect("check"),
        StoredClosureCheck::Invalid { .. }
    ));

    // Single-file vs multi-file plan_stored_closure + read_stored_flow_files.
    tmp.write("src/single.yaml", "- tapOn: ok\n");
    let single_closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("src/single.yaml")).expect("closure");
    let single_plan = plan_stored_closure(&single_closure, "today", "ios");
    assert!(single_plan.flow_path.ends_with(".yaml"));
    assert!(single_plan.subflow_files.is_empty());

    let valid_json = serde_json::json!([
        { "path": ".deja/flows/g/a.yaml", "sha256": "a".repeat(64) }
    ]);
    assert_eq!(
        read_stored_flow_files(&valid_json),
        Some(vec![StoredFlowFile {
            path: ".deja/flows/g/a.yaml".into(),
            sha256: "a".repeat(64),
        }])
    );
    let bad_json = serde_json::json!([
        { "path": "../escape.yaml", "sha256": "a".repeat(64) }
    ]);
    assert_eq!(read_stored_flow_files(&bad_json), None);
}

#[test]
fn deja38_l2_closure_assertions_memoization_and_cap() {
    assert_eq!(MAX_CLOSURE_ASSERTIONS, 50_000);

    // Shallow diamond DAG: l0 calls l1 x100 -> l1 calls l2 x50 -> l2 has 12 assertions.
    // 100 * 50 * 12 = 60,000 leaf lines (~5 MiB < 16 MiB byte budget) -> cleanly hits MAX_CLOSURE_ASSERTIONS!
    let tmp = TempDir::new("deja38-l2");
    tmp.write(
        "flows/l0.yaml",
        (0..100).map(|_| "- runFlow: l1.yaml\n").collect::<String>(),
    );
    tmp.write(
        "flows/l1.yaml",
        (0..50).map(|_| "- runFlow: l2.yaml\n").collect::<String>(),
    );
    let leaf = (0..12)
        .map(|i| format!("- assertVisible:\n    id: \"a{i}\"\n"))
        .collect::<String>();
    tmp.write("flows/l2.yaml", leaf);

    let closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("flows/l0.yaml")).expect("closure");
    assert_eq!(closure.files.len(), 3);
    let err = match closure_assertions(&closure) {
        Ok(items) => panic!(
            "expected MAX_CLOSURE_ASSERTIONS cap error, got {} items",
            items.len()
        ),
        Err(err) => err,
    };
    assert!(matches!(err, CoreError::FlowClosure(_)));
    assert!(err.to_string().contains("50000"));
}

#[test]
fn check_stored_subflows_walks_extensionless_flows_and_skips_yaml_named_media() {
    let sha = |bytes: &[u8]| hex::encode(Sha256::digest(bytes));

    // 1. Extensionless subflow referencing an unlisted script must be inspected and rejected.
    let tmp = TempDir::new("extensionless-subflow");
    let main_bytes = b"- runFlow: subflow_no_ext\n";
    let sub_bytes = b"- runScript: unlisted.js\n";
    tmp.write(".deja/flows/g/ios-aa/main.yaml", main_bytes);
    tmp.write(".deja/flows/g/ios-aa/subflow_no_ext", sub_bytes);
    tmp.write(".deja/flows/g/ios-aa/unlisted.js", b"output.ok = true\n");

    let listed = vec![StoredFlowFile {
        path: ".deja/flows/g/ios-aa/subflow_no_ext".into(),
        sha256: sha(sub_bytes),
    }];
    let res = check_stored_subflows(
        tmp.path(),
        ".deja/flows/g/ios-aa/main.yaml",
        Some(&listed),
        false,
    )
    .expect("check");
    assert!(
        matches!(res, StoredClosureCheck::Invalid { ref message } if message.contains("unlisted.js")),
        "expected extensionless subflow to catch unlisted script, got {res:?}"
    );

    // 2. Binary media file named `blob.yaml` must NOT be parsed as YAML.
    let media_tmp = TempDir::new("yaml-named-media");
    let media_main = b"- addMedia:\n    - blob.yaml\n";
    let binary_blob: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0xff, 0x00, b'[', b'{', b':'];
    media_tmp.write(".deja/flows/g/ios-bb/main.yaml", media_main);
    media_tmp.write(".deja/flows/g/ios-bb/blob.yaml", binary_blob);
    let media_listed = vec![StoredFlowFile {
        path: ".deja/flows/g/ios-bb/blob.yaml".into(),
        sha256: sha(binary_blob),
    }];
    assert_eq!(
        check_stored_subflows(
            media_tmp.path(),
            ".deja/flows/g/ios-bb/main.yaml",
            Some(&media_listed),
            false,
        )
        .expect("check"),
        StoredClosureCheck::Ok
    );
}

#[test]
fn invalid_utf8_in_yaml_comment_is_consistent_between_collect_and_check_stored() {
    let sha = |bytes: &[u8]| hex::encode(Sha256::digest(bytes));
    let tmp = TempDir::new("lossy-utf8");
    let mut main_bytes = b"appId: com.example\n# comment with invalid utf8: ".to_vec();
    main_bytes.push(0xff);
    main_bytes.extend_from_slice(b"\n---\n- runFlow: sub.yaml\n");
    let mut sub_bytes = b"# sub invalid utf8 ".to_vec();
    sub_bytes.push(0xfe);
    sub_bytes.extend_from_slice(b"\n- tapOn: ok\n");

    tmp.write(".deja/flows/g/ios-cc/main.yaml", &main_bytes);
    tmp.write(".deja/flows/g/ios-cc/sub.yaml", &sub_bytes);

    let closure = collect_flow_closure(
        tmp.path(),
        &tmp.path().join(".deja/flows/g/ios-cc/main.yaml"),
    )
    .expect("collect_flow_closure accepts lossy utf8 in comment");
    assert_eq!(closure.files.len(), 2);

    let listed = vec![StoredFlowFile {
        path: ".deja/flows/g/ios-cc/sub.yaml".into(),
        sha256: sha(&sub_bytes),
    }];
    assert_eq!(
        check_stored_subflows(
            tmp.path(),
            ".deja/flows/g/ios-cc/main.yaml",
            Some(&listed),
            false,
        )
        .expect("check_stored_subflows agrees with collect_flow_closure"),
        StoredClosureCheck::Ok
    );
}

#[test]
fn closure_assertions_enforces_byte_budget_on_deep_linear_call_chain() {
    assert_eq!(MAX_CLOSURE_ASSERTION_BYTES, 16 * 1024 * 1024);

    // Linear 15-deep chain where level 0 attaches a 4 KiB env payload to runFlow.
    // Because `chain` embeds each prior line verbatim, line size doubles per hop (~4 KiB * 2^14 = 64 MiB),
    // tripping MAX_CLOSURE_ASSERTION_BYTES well before exhausting memory.
    let tmp = TempDir::new("linear-byte-cap");
    let pad = "x".repeat(4096);
    tmp.write(
        "flows/f0.yaml",
        format!("- runFlow:\n    file: f1.yaml\n    env:\n      PAD: \"{pad}\"\n"),
    );
    for level in 1..15 {
        tmp.write(
            &format!("flows/f{level}.yaml"),
            format!("- runFlow: f{}.yaml\n", level + 1),
        );
    }
    tmp.write("flows/f15.yaml", "- assertVisible:\n    id: leaf\n");

    let closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("flows/f0.yaml")).expect("closure");
    let err = match closure_assertions(&closure) {
        Ok(items) => panic!("expected byte cap error, got {} items", items.len()),
        Err(err) => err,
    };
    assert!(matches!(err, CoreError::FlowClosure(_)));
    assert!(err.to_string().contains("bytes"));
}

#[test]
fn closure_enforces_longest_path_depth_through_shared_subflow_regardless_of_call_order() {
    // main calls shared.yaml directly (depth 1 -> shared -> leaf depth 2) AND via a 15-deep chain
    // c1 -> c2 -> ... -> c15 -> shared.yaml -> leaf.yaml (total depth 16 >= MAX_DEPTH).
    // Even though shared.yaml is visited first at shallow depth 1, the longest path must fail closed!
    for shared_first in [true, false] {
        let tmp = TempDir::new("shared-depth");
        let main_yaml = if shared_first {
            "- runFlow: shared.yaml\n- runFlow: c1.yaml\n"
        } else {
            "- runFlow: c1.yaml\n- runFlow: shared.yaml\n"
        };
        tmp.write("flows/main.yaml", main_yaml);
        for i in 1..15 {
            tmp.write(
                &format!("flows/c{i}.yaml"),
                format!("- runFlow: c{}.yaml\n", i + 1),
            );
        }
        tmp.write("flows/c15.yaml", "- runFlow: shared.yaml\n");
        tmp.write("flows/shared.yaml", "- runFlow: leaf.yaml\n");
        tmp.write("flows/leaf.yaml", "- tapOn: ok\n");

        let err = collect_flow_closure(tmp.path(), &tmp.path().join("flows/main.yaml"))
            .expect_err("longest path depth 16 must fail regardless of call order");
        assert!(matches!(err, CoreError::FlowClosure(_)));
        assert!(
            err.to_string().contains("nested more than 16 deep"),
            "unexpected error (shared_first={shared_first}): {err}"
        );
    }
}

#[test]
fn flow_references_closure_and_assertions_trim_es_bom_whitespace() {
    let yaml = "- runFlow: \"\u{FEFF}child.yaml\u{FEFF}\"\n- addMedia: [\"\u{FEFF}pic.png\"]\n";
    let refs = find_flow_references(yaml).expect("refs");
    assert_eq!(refs[0].path, "child.yaml");
    assert_eq!(refs[1].path, "pic.png");

    let assertions = deja_core::find_flow_assertions(yaml).expect("assertions");
    assert_eq!(
        assertions[0].call.as_ref().map(|c| c.path.as_str()),
        Some("child.yaml")
    );

    let tmp = TempDir::new("es-trim-bom");
    tmp.write("flows/main.yaml", yaml);
    tmp.write("flows/child.yaml", "- assertVisible:\n    id: ok\n");
    tmp.write("flows/pic.png", [0x89u8, 0x50, 0x4e, 0x47]);
    let closure =
        collect_flow_closure(tmp.path(), &tmp.path().join("flows/main.yaml")).expect("closure");
    assert_eq!(closure.files.len(), 3);
    assert_eq!(
        closure_assertions(&closure)
            .expect("closure assertions")
            .len(),
        2
    );
}

#[test]
#[cfg(unix)]
fn check_stored_subflows_rejects_unconfined_main_flow_symlink_and_oversized_files() {
    use deja_core::{StoredClosureCheck, StoredFlowFile, check_stored_subflows};
    use std::os::unix::fs::symlink;

    let outside = TempDir::new("outside-secret");
    outside.write("secret.yaml", "- tapOn: ok\n");

    let tmp = TempDir::new("stored-confinement");
    std::fs::create_dir_all(tmp.path().join(".deja/flows")).expect("mkdir .deja/flows");
    symlink(
        outside.path().join("secret.yaml"),
        tmp.path().join(".deja/flows/main.yaml"),
    )
    .expect("symlink main");

    // Both legacy branch (None) and non-legacy branch (Some(&[])) must reject a symlinked main flow.
    let legacy =
        check_stored_subflows(tmp.path(), ".deja/flows/main.yaml", None, true).expect("legacy");
    assert!(matches!(legacy, StoredClosureCheck::Invalid { .. }));

    let modern =
        check_stored_subflows(tmp.path(), ".deja/flows/main.yaml", Some(&[]), false).expect("mod");
    assert!(matches!(modern, StoredClosureCheck::Invalid { .. }));

    // Oversized stored subflow file (> 8 MiB MAX_MEDIA_BYTES) fails bounded checksum verification.
    let tmp_big = TempDir::new("stored-oversized");
    tmp_big.write(".deja/flows/main.yaml", "- addMedia: [big.bin]\n");
    tmp_big.write(
        ".deja/flows/big.bin",
        vec![b'a'; (deja_core::MAX_MEDIA_BYTES as usize) + 16],
    );
    let sub = vec![StoredFlowFile {
        path: ".deja/flows/big.bin".into(),
        sha256: "0".repeat(64),
    }];
    let res = check_stored_subflows(tmp_big.path(), ".deja/flows/main.yaml", Some(&sub), false)
        .expect("oversized check");
    assert!(
        matches!(res, StoredClosureCheck::Invalid { ref message } if message.contains("limit")),
        "expected size-limit Invalid, got {res:?}"
    );
}
