use deja_cli::repo_check::{RepoCheckOptions, render_results_chart_svg, validate_repo};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repository root should exist")
}

struct TempGitRepo {
    path: PathBuf,
}

impl TempGitRepo {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "deja-repo-check-{name}-{}-{nanos}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp git repo dir");
        run_git(&path, &["init", "-q"]);
        run_git(&path, &["config", "user.name", "Deja Test"]);
        run_git(&path, &["config", "user.email", "deja-test@example.com"]);
        Self { path }
    }

    fn write(&self, rel: &str, contents: &[u8]) {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&full, contents).expect("write fixture file");
    }

    fn commit_all(&self, msg: &str) {
        run_git(&self.path, &["add", "-A"]);
        run_git(&self.path, &["commit", "-q", "-m", msg]);
    }
}

impl Drop for TempGitRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn live_repo_passes_artifact_and_chart_validation() {
    let root = repo_root();
    let report = validate_repo(
        &root,
        RepoCheckOptions {
            require_clean: false,
            check_chart: true,
        },
    )
    .expect("live repository should pass deja-repo-check");

    assert_eq!(report.result, "pass");
    assert!(report.generated_artifacts_absent);
    assert!(report.markdown_links_valid);
    assert_eq!(report.results_chart_valid, Some(true));
    assert!(report.tracked_files > 10);
    assert!(report.markdown_links > 10);

    let expected_svg = render_results_chart_svg(&root).expect("render chart svg");
    let committed_svg =
        fs::read_to_string(root.join("docs/results-chart.svg")).expect("read committed svg");
    assert_eq!(
        committed_svg, expected_svg,
        "pure-Rust chart renderer must match docs/results-chart.svg byte-for-byte"
    );
}

#[test]
fn rejects_forbidden_tracked_artifact_paths() {
    let repo = TempGitRepo::new("forbidden-path");
    repo.write("README.md", b"# Test\n");
    repo.write("target/debug/deja", b"not-binary");
    repo.commit_all("initial");

    let err = validate_repo(&repo.path, RepoCheckOptions::default())
        .expect_err("tracked target/ artifact must fail");
    assert!(err.contains("target/debug/deja"), "unexpected error: {err}");
}

#[test]
fn rejects_broken_local_markdown_links() {
    let repo = TempGitRepo::new("broken-link");
    repo.write("README.md", b"See [missing](docs/does-not-exist.md).\n");
    repo.commit_all("initial");

    let err = validate_repo(&repo.path, RepoCheckOptions::default())
        .expect_err("broken local Markdown link must fail");
    assert!(
        err.contains("README.md: docs/does-not-exist.md"),
        "unexpected error: {err}"
    );
}

#[test]
fn rejects_invalid_json_and_secret_leaks() {
    let repo_json = TempGitRepo::new("bad-json");
    repo_json.write("config.json", b"{ invalid json");
    repo_json.commit_all("initial");
    let err = validate_repo(&repo_json.path, RepoCheckOptions::default())
        .expect_err("invalid JSON must fail");
    assert!(err.contains("Invalid JSON in config.json"), "{err}");

    let repo_leak = TempGitRepo::new("secret-leak");
    let fake_token = format!("token = {}123456789012345678901234\n", "ghp_");
    repo_leak.write("notes.txt", fake_token.as_bytes());
    repo_leak.commit_all("initial");
    let err = validate_repo(&repo_leak.path, RepoCheckOptions::default())
        .expect_err("GitHub token leak must fail");
    assert!(err.contains("GitHub credential"), "{err}");

    // Preserve ASCII word-boundary parity with JS/Bun regexes: a token immediately following
    // a non-ASCII Unicode letter must still be detected as a leak.
    let repo_unicode_leak = TempGitRepo::new("unicode-secret-leak");
    let unicode_prefixed_token = format!("clé{}123456789012345678901234\n", "ghp_");
    repo_unicode_leak.write("notes.txt", unicode_prefixed_token.as_bytes());
    repo_unicode_leak.commit_all("initial");
    let err = validate_repo(&repo_unicode_leak.path, RepoCheckOptions::default())
        .expect_err("Unicode-prefixed GitHub token must still fail ASCII-boundary scan");
    assert!(err.contains("GitHub credential"), "{err}");
}

#[test]
fn rejects_subdirectory_root_for_repository_wide_check() {
    let repo = TempGitRepo::new("subdir-root");
    repo.write("README.md", b"See [missing](docs/nope.md).\n");
    repo.write("sub/ok.txt", b"clean\n");
    repo.commit_all("initial");

    let err = validate_repo(&repo.path.join("sub"), RepoCheckOptions::default())
        .expect_err("subdirectory --root must be rejected");
    assert!(
        err.contains("repository top level"),
        "unexpected error: {err}"
    );
}

#[test]
fn require_clean_rejects_dirty_worktree() {
    let repo = TempGitRepo::new("dirty");
    repo.write("README.md", b"# Clean\n");
    repo.commit_all("initial");
    repo.write("untracked.txt", b"dirty");

    let err = validate_repo(
        &repo.path,
        RepoCheckOptions {
            require_clean: true,
            check_chart: false,
        },
    )
    .expect_err("dirty checkout must fail with require_clean");
    assert!(err.contains("untracked.txt"), "{err}");
}
