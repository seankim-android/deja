//! Pure-Rust repository artifact validator and results-chart SVG generator.
//!
//! Replaces `scripts/validate-release-artifacts.ts` and `scripts/render-results-chart.ts`
//! with zero Bun/Node runtime dependency.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Structured validation summary emitted as JSON by `deja-repo-check`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepoCheckReport {
    pub result: String,
    pub head: String,
    pub tracked_files: usize,
    pub text_files: usize,
    pub json_files: usize,
    pub generated_artifacts_absent: bool,
    pub markdown_links: usize,
    pub markdown_links_valid: bool,
    pub secret_and_machine_path_scan: String,
    pub checkout_unchanged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results_chart_valid: Option<bool>,
}

/// Options controlling [`validate_repo`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RepoCheckOptions {
    pub require_clean: bool,
    pub check_chart: bool,
}

/// Finds the repository root containing `dir` via `git rev-parse --show-toplevel`.
pub fn discover_repo_root(start_dir: &Path) -> Result<PathBuf, String> {
    let out = run_git(start_dir, &["rev-parse", "--show-toplevel"])?;
    let trimmed = out.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Err("git rev-parse --show-toplevel returned an empty path".to_string());
    }
    Ok(PathBuf::from(trimmed))
}

fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|err| format!("failed to run git {}: {err}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn forbidden_path_patterns() -> Vec<Regex> {
    [
        r"(^|/)\.deja/",
        r"(^|/)\.tmp/",
        r"(^|/)node_modules/",
        r"(^|/)dist/",
        r"(^|/)\.gradle/",
        r"\.(apk|aab|class|pyc)$",
        r"(^|/)(build|dist|out)/.*\.jar$",
        r"(^|/)demo/assets/",
        r"(^|/)demo/rehearsal/scene-[0-9]+\.png$",
        r"(^|/)target/",
    ]
    .into_iter()
    .map(|pat| Regex::new(pat).expect("valid static forbidden path regex"))
    .collect()
}

fn forbidden_content_patterns() -> Vec<(&'static str, Regex)> {
    // Split sensitive literal tokens across concat! so scanning this source file never self-triggers.
    [
        ("private home path", concat!("/Users/", "seankim")),
        ("private key", concat!("BEGIN ", "[A-Z ]*", "PRIVATE KEY")),
        (
            "GitHub credential",
            concat!(r"(?-u:\b)gh", "[opusr]_[A-Za-z0-9]{20,}"),
        ),
        (
            "OpenAI-shaped credential",
            concat!(r"(?-u:\b)sk", "-[A-Za-z0-9_-]{24,}"),
        ),
    ]
    .into_iter()
    .map(|(label, pat)| (label, Regex::new(pat).expect("valid static leak regex")))
    .collect()
}

fn is_external_scheme(target: &str) -> bool {
    let Some((scheme, _)) = target.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// Validates tracked repository artifacts, Markdown links, JSON syntax, secret leaks,
/// optional clean working tree, and optional results-chart SVG parity.
pub fn validate_repo(root: &Path, options: RepoCheckOptions) -> Result<RepoCheckReport, String> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|err| format!("failed to resolve root {}: {err}", root.display()))?;
    let toplevel = discover_repo_root(&canonical_root)?;
    let canonical_toplevel = fs::canonicalize(&toplevel).map_err(|err| {
        format!(
            "failed to resolve git toplevel {}: {err}",
            toplevel.display()
        )
    })?;
    if canonical_root != canonical_toplevel {
        return Err(format!(
            "--root must point to the repository top level ({}), not a subdirectory ({})",
            canonical_toplevel.display(),
            canonical_root.display()
        ));
    }

    let raw_files = run_git(&canonical_root, &["ls-files", "-z"])?;
    let files: Vec<&str> = raw_files.split('\0').filter(|s| !s.is_empty()).collect();

    let forbidden_paths = forbidden_path_patterns();
    let forbidden_tracked: Vec<&str> = files
        .iter()
        .copied()
        .filter(|path| forbidden_paths.iter().any(|re| re.is_match(path)))
        .collect();
    if !forbidden_tracked.is_empty() {
        return Err(format!(
            "Generated or ignored artifacts are tracked: {}",
            forbidden_tracked.join(", ")
        ));
    }

    let md_link_re = Regex::new(r"!?\[[^\]\n]*\]\(([^)\n]+)\)").expect("valid markdown link regex");
    let forbidden_contents = forbidden_content_patterns();

    let mut text_files = 0_usize;
    let mut json_files = 0_usize;
    let mut markdown_links = 0_usize;
    let mut leaks: Vec<String> = Vec::new();
    let mut broken_markdown_links: Vec<String> = Vec::new();

    for rel_path in &files {
        let full_path = root.join(rel_path);
        let bytes = fs::read(&full_path)
            .map_err(|err| format!("failed to read tracked file {rel_path}: {err}"))?;
        if bytes.contains(&0) {
            continue;
        }
        let contents = String::from_utf8_lossy(&bytes);
        text_files += 1;

        if rel_path.ends_with(".md") {
            let parent_dir = Path::new(rel_path)
                .parent()
                .map(|p| root.join(p))
                .unwrap_or_else(|| root.to_path_buf());
            for caps in md_link_re.captures_iter(&contents) {
                let raw_target = caps.get(1).map(|m| m.as_str()).unwrap_or("").trim();
                let target = raw_target
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if target.is_empty()
                    || is_external_scheme(target)
                    || target.starts_with('#')
                    || target.contains("${")
                    || target.contains('*')
                {
                    continue;
                }
                let local_path = target.split('#').next().unwrap_or("");
                if local_path.is_empty() {
                    continue;
                }
                markdown_links += 1;
                let resolved = if let Some(stripped) = local_path.strip_prefix('/') {
                    root.join(stripped)
                } else {
                    parent_dir.join(local_path)
                };
                if !resolved.exists() {
                    broken_markdown_links.push(format!("{rel_path}: {target}"));
                }
            }
        }

        // Legacy TypeScript tsconfig files are JSONC (comments/trailing commas).
        if rel_path.ends_with(".json") && !rel_path.starts_with("tsconfig") {
            serde_json::from_str::<serde_json::Value>(&contents)
                .map_err(|err| format!("Invalid JSON in {rel_path}: {err}"))?;
            json_files += 1;
        }

        for (label, re) in &forbidden_contents {
            if re.is_match(&contents) {
                leaks.push(format!("{rel_path}: {label}"));
            }
        }
    }

    if !leaks.is_empty() {
        return Err(format!(
            "Potential secret/machine leaks:\n{}",
            leaks.join("\n")
        ));
    }
    if !broken_markdown_links.is_empty() {
        return Err(format!(
            "Broken local Markdown links:\n{}",
            broken_markdown_links.join("\n")
        ));
    }

    let results_chart_valid = if options.check_chart {
        let expected = render_results_chart_svg(root)?;
        let chart_path = root.join("docs/results-chart.svg");
        let current = fs::read_to_string(&chart_path).unwrap_or_default();
        if current != expected {
            return Err(
                "docs/results-chart.svg is out of date; run `cargo run --bin deja-repo-check -- --render-chart`"
                    .to_string(),
            );
        }
        Some(true)
    } else {
        None
    };

    let dirty = run_git(
        &canonical_root,
        &["status", "--porcelain", "--untracked-files=normal"],
    )?;
    let checkout_unchanged = dirty.trim().is_empty();
    if options.require_clean && !checkout_unchanged {
        return Err(format!(
            "Release checkout changed during validation:\n{}",
            dirty.trim()
        ));
    }

    let head = run_git(&canonical_root, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    Ok(RepoCheckReport {
        result: "pass".to_string(),
        head,
        tracked_files: files.len(),
        text_files,
        json_files,
        generated_artifacts_absent: true,
        markdown_links,
        markdown_links_valid: true,
        secret_and_machine_path_scan: "pass".to_string(),
        checkout_unchanged,
        results_chart_valid,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunReport {
    model_calls: u64,
    duration_ms: f64,
    #[serde(default)]
    repair: Option<RepairTiming>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairTiming {
    duration_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartRow {
    pub label: &'static str,
    pub model_calls: u64,
    pub seconds: f64,
}

const CHART_RUNS: &[(&str, &str)] = &[
    ("Android cold explore", "android/cold-run-report.json"),
    ("Android warm replay", "android/warm-run-report.json"),
    (
        "Android stale selector repair",
        "android/stale-repair-run-report.json",
    ),
    ("Chromium warm replay", "chromium/run-run-report.json"),
];

fn js_round(x: f64) -> f64 {
    // Match ECMAScript Math.round for non-negative durations (round half toward +inf).
    (x + 0.5).floor()
}

fn bar_path(x: f64, y: f64, w: f64, h: f64) -> String {
    if w <= 0.0 {
        return String::new();
    }
    let r = 4.0_f64.min(w / 2.0).min(h / 2.0);
    format!(
        "M{x},{y}H{x_r}Q{x_w},{y} {x_w},{y_r}V{y_h_r}Q{x_w},{y_h} {x_r},{y_h}H{x}Z",
        x_r = x + w - r,
        x_w = x + w,
        y_r = y + r,
        y_h_r = y + h - r,
        y_h = y + h,
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;")
}

/// Loads the saved live-evidence run reports used by `docs/results-chart.svg`.
pub fn load_chart_rows(root: &Path) -> Result<Vec<ChartRow>, String> {
    let mut rows = Vec::with_capacity(CHART_RUNS.len());
    for &(label, rel) in CHART_RUNS {
        let path = root.join("demo/live-evidence").join(rel);
        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let report: RunReport = serde_json::from_str(&raw)
            .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
        let duration_ms = report
            .repair
            .as_ref()
            .map(|r| r.duration_ms)
            .unwrap_or(report.duration_ms);
        let seconds = js_round(duration_ms / 100.0) / 10.0;
        rows.push(ChartRow {
            label,
            model_calls: report.model_calls,
            seconds,
        });
    }
    Ok(rows)
}

/// Renders the deterministic `docs/results-chart.svg` markup from `demo/live-evidence/`.
pub fn render_results_chart_svg(root: &Path) -> Result<String, String> {
    let rows = load_chart_rows(root)?;
    let width: f64 = 820.0;
    let top: f64 = 100.0;
    let row_height: f64 = 46.0;
    let bar_height: f64 = 16.0;
    let plot_bottom = top + (rows.len() as f64) * row_height;
    let height = plot_bottom + 64.0;

    let desc_rows: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{}: {} model calls, {} s",
                r.label, r.model_calls, r.seconds
            )
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" role=\"img\" aria-labelledby=\"t d\">"
    ));
    parts.push("<title id=\"t\">Déjà run results: model calls and duration</title>".to_string());
    parts.push(format!(
        "<desc id=\"d\">{}. Source: demo/live-evidence run reports.</desc>",
        desc_rows.join(". ")
    ));
    parts.push(
        "<style>\n  .ink { fill: #0b0b0b } .muted { fill: #52514e } .grid { stroke: #a9a8a2 } .axis { stroke: #a9a8a2 } .bar { fill: #2a78d6 }\n  text { font-family: -apple-system, BlinkMacSystemFont, \"Segoe UI\", Helvetica, Arial, sans-serif }\n  @media (prefers-color-scheme: dark) {\n    .ink { fill: #f0efec } .muted { fill: #c3c2b7 } .grid { stroke: #6b6a64 } .axis { stroke: #6b6a64 } .bar { fill: #3987e5 }\n  }\n</style>"
            .to_string(),
    );
    parts.push(
        "<text class=\"ink\" x=\"0\" y=\"22\" font-size=\"16\" font-weight=\"600\">Replay and repair make 0 model calls</text>"
            .to_string(),
    );
    parts.push(
        "<text class=\"muted\" x=\"0\" y=\"46\" font-size=\"12\">Saved run reports in demo/live-evidence. Different run types, so durations are not a speedup claim.</text>"
            .to_string(),
    );

    for (i, row) in rows.iter().enumerate() {
        let y = top + (i as f64) * row_height + row_height / 2.0;
        parts.push(format!(
            "<text class=\"ink\" x=\"0\" y=\"{}\" font-size=\"13\">{}</text>",
            y + 4.0,
            xml_escape(row.label)
        ));
    }

    struct Panel {
        title: &'static str,
        x: f64,
        width: f64,
        max: f64,
        ticks: &'static [u64],
        is_duration: bool,
    }

    let panels = [
        Panel {
            title: "Model calls",
            x: 236.0,
            width: 180.0,
            max: 15.0,
            ticks: &[0, 5, 10, 15],
            is_duration: false,
        },
        Panel {
            title: "Duration (s)",
            x: 490.0,
            width: 230.0,
            max: 100.0,
            ticks: &[0, 25, 50, 75, 100],
            is_duration: true,
        },
    ];

    for panel in panels {
        parts.push(format!(
            "<text class=\"muted\" x=\"{}\" y=\"{}\" font-size=\"12\" font-weight=\"600\">{}</text>",
            panel.x,
            top - 14.0,
            panel.title
        ));
        for &tick in panel.ticks {
            let x = panel.x + ((tick as f64) / panel.max) * panel.width;
            if tick == 0 {
                parts.push(format!(
                    "<line class=\"axis\" x1=\"{x}\" x2=\"{x}\" y1=\"{top}\" y2=\"{plot_bottom}\" stroke-width=\"1\"/>"
                ));
            } else {
                parts.push(format!(
                    "<line class=\"grid\" x1=\"{x}\" x2=\"{x}\" y1=\"{plot_bottom}\" y2=\"{}\" stroke-width=\"1\"/>",
                    plot_bottom + 4.0
                ));
            }
            parts.push(format!(
                "<text class=\"muted\" x=\"{x}\" y=\"{}\" font-size=\"11\" text-anchor=\"middle\">{tick}</text>",
                plot_bottom + 18.0
            ));
        }

        for (i, row) in rows.iter().enumerate() {
            let value = if panel.is_duration {
                row.seconds
            } else {
                row.model_calls as f64
            };
            let y = top + (i as f64) * row_height + (row_height - bar_height) / 2.0;
            let w = (value / panel.max) * panel.width;
            if w > 0.0 {
                parts.push(format!(
                    "<path class=\"bar\" d=\"{}\"/>",
                    bar_path(panel.x, y, w, bar_height)
                ));
            }
            let formatted = if panel.is_duration {
                format!("{value:.1} s")
            } else {
                format!("{}", row.model_calls)
            };
            parts.push(format!(
                "<text class=\"ink\" x=\"{}\" y=\"{}\" font-size=\"12\">{formatted}</text>",
                panel.x + w + 6.0,
                y + bar_height / 2.0 + 4.0
            ));
        }
    }

    let generator = if root.join("scripts/render-results-chart.ts").exists() {
        "scripts/render-results-chart.ts"
    } else {
        "deja-repo-check"
    };
    parts.push(format!(
        "<text class=\"muted\" x=\"0\" y=\"{}\" font-size=\"11\">Repair duration is the repair step only. Generated by {generator}.</text>",
        height - 8.0
    ));
    parts.push("</svg>".to_string());
    Ok(format!("{}\n", parts.join("\n")))
}

/// Regenerates `docs/results-chart.svg` on disk and returns the parsed chart rows.
pub fn write_results_chart_svg(root: &Path) -> Result<Vec<ChartRow>, String> {
    let rows = load_chart_rows(root)?;
    let svg = render_results_chart_svg(root)?;
    let output = root.join("docs/results-chart.svg");
    fs::write(&output, svg)
        .map_err(|err| format!("failed to write {}: {err}", output.display()))?;
    Ok(rows)
}
