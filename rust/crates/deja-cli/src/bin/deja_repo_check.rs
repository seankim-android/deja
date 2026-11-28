//! Pure-Rust repository artifact, Markdown link, leak, and results-chart validator.

use clap::Parser;
use deja_cli::repo_check::{
    RepoCheckOptions, discover_repo_root, validate_repo, write_results_chart_svg,
};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "deja-repo-check",
    version,
    about = "Validate tracked repository artifacts, links, leaks, and results-chart SVG"
)]
struct Args {
    /// Repository root directory (defaults to git toplevel of current directory).
    #[arg(long)]
    root: Option<PathBuf>,

    /// Fail if `git status --porcelain` reports any uncommitted changes.
    #[arg(long)]
    require_clean: bool,

    /// Verify `docs/results-chart.svg` matches `demo/live-evidence/` run reports.
    #[arg(long)]
    check_chart: bool,

    /// Regenerate `docs/results-chart.svg` from `demo/live-evidence/` run reports and exit.
    #[arg(long, conflicts_with_all = ["check_chart", "require_clean"])]
    render_chart: bool,
}

fn run() -> Result<String, String> {
    let args = Args::parse();
    let root = match args.root {
        Some(path) => path,
        None => {
            let cwd =
                std::env::current_dir().map_err(|err| format!("failed to read cwd: {err}"))?;
            discover_repo_root(&cwd)?
        }
    };

    if args.render_chart {
        let rows = write_results_chart_svg(&root)?;
        return serde_json::to_string_pretty(&rows)
            .map_err(|err| format!("failed to serialize chart rows: {err}"));
    }

    let report = validate_repo(
        &root,
        RepoCheckOptions {
            require_clean: args.require_clean,
            check_chart: args.check_chart,
        },
    )?;
    serde_json::to_string_pretty(&report)
        .map_err(|err| format!("failed to serialize report: {err}"))
}

fn main() -> ExitCode {
    match run() {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("deja-repo-check error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn render_chart_conflicts_with_require_clean_and_check_chart() {
        assert!(
            Args::try_parse_from(["deja-repo-check", "--render-chart", "--require-clean"]).is_err()
        );
        assert!(
            Args::try_parse_from(["deja-repo-check", "--render-chart", "--check-chart"]).is_err()
        );
        assert!(
            Args::try_parse_from(["deja-repo-check", "--require-clean", "--check-chart"]).is_ok()
        );
    }
}
