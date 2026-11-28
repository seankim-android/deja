//! Clap command-line definition for `deja`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Top-level `deja` CLI arguments.
#[derive(Debug, Parser)]
#[command(
    name = "deja",
    version,
    about = "Verified app test memory: explore once, replay with zero model calls"
)]
pub struct Cli {
    /// Path to `deja.config.json` (default: `./deja.config.json`).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Emit machine-readable JSON on stdout.
    #[arg(long, global = true)]
    pub json: bool,

    /// Bypass the persistent warm driver daemon and run drivers directly in-process.
    #[arg(long, global = true)]
    pub no_daemon: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Top-level `deja` subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Replay a remembered walk on the current build with zero model calls.
    Run(RunArgs),

    /// Execute a reviewed YAML flow and persist it only on a complete pass.
    Remember(RememberArgs),

    /// List valid local walks without invoking a driver or model.
    List(ListArgs),

    /// Inspect a saved JSON report file or a goal's validation history from the store.
    Inspect(InspectArgs),

    /// Static checks on consumer-owned flow manifests (`deja.walks.json`).
    Manifest(ManifestArgs),

    /// Read-only release gate verifying every required goal passed on this exact build.
    Gate(GateArgs),

    /// Read-only environment and native toolchain diagnostics (no Java or Maestro required).
    Doctor(DoctorArgs),

    /// Import legacy `walks.json` into SQLite v5 (read-only on `walks.json`).
    Migrate(MigrateArgs),

    /// Print the embedded Déjà agent workflow guide (`docs/AGENT_GUIDE.md`).
    Guide,

    /// Legacy engine command (Déjà now executes flows natively without Maestro).
    Engine(EngineArgs),

    /// Start the stdio JSON-RPC 2.0 MCP server confined to this workspace and app.
    Mcp(McpArgs),

    /// Manage the persistent warm driver daemon (`start`, `status`, `stop`).
    Daemon(DaemonArgs),
}

/// Arguments for `deja run [<goal>... | --goals <FILE> | --all]`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Goal to replay (use `--` before goals starting with `-`).
    #[arg(num_args = 0.., value_name = "GOAL")]
    pub goal: Vec<String>,

    /// Run a batch of goals from a file (plain `#`-comment lines, JSON array, or `deja.walks.json`).
    #[arg(long, value_name = "FILE")]
    pub goals: Option<PathBuf>,

    /// Run all matching goals from the configured `deja.walks.json` manifest.
    #[arg(long)]
    pub all: bool,

    /// Comma-separated target device / simulator IDs for dynamic multi-device sharding.
    #[arg(long, value_delimiter = ',', value_name = "IDS")]
    pub devices: Vec<String>,

    /// Re-verify a changed or unvalidated consumer flow in manifest mode.
    #[arg(long)]
    pub verify: bool,

    /// Rerun a failed replay once on the exact same bytes to detect flakiness (exit 7).
    #[arg(long)]
    pub rerun_failed: bool,

    /// Number of additional retry attempts after an initial failure (`--retries 1` matches `--rerun-failed`).
    #[arg(long, value_name = "N")]
    pub retries: Option<u8>,

    /// Override target device / simulator UDID (`--device` > `DEJA_DEVICE` > `config.device`).
    #[arg(long, value_name = "ID")]
    pub device: Option<String>,

    /// Isolated store identifier under `.deja/memory/stores/<id>/`.
    #[arg(long, value_name = "ID")]
    pub store: Option<String>,

    /// Write an additional copy of the redacted run report JSON to this path.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Write a redacted JUnit XML report to this path.
    #[arg(long, value_name = "PATH")]
    pub junit: Option<PathBuf>,

    /// Emit a single-line `deja-summary v=1` trailer.
    #[arg(long)]
    pub summary: bool,

    /// Hidden trap so `deja run --flow ...` emits a clear exit-2 usage message.
    #[arg(long, hide = true, value_name = "FILE")]
    pub flow: Option<PathBuf>,
}

/// Arguments for `deja remember <goal>... --flow <candidate.yaml>`.
#[derive(Debug, Args)]
pub struct RememberArgs {
    /// Goal to remember (use `--` before goals starting with `-`).
    #[arg(required = true, num_args = 1.., value_name = "GOAL")]
    pub goal: Vec<String>,

    /// Candidate `maestro/v1` YAML flow to verify and remember.
    #[arg(long, value_name = "FILE")]
    pub flow: Option<PathBuf>,

    /// Accept reviewed assertion changes in manifest mode.
    #[arg(long)]
    pub accept_assertion_change: bool,

    /// Override target device / simulator UDID (`--device` > `DEJA_DEVICE` > `config.device`).
    #[arg(long, value_name = "ID")]
    pub device: Option<String>,

    /// Isolated store identifier under `.deja/memory/stores/<id>/`.
    #[arg(long, value_name = "ID")]
    pub store: Option<String>,

    /// Write an additional copy of the redacted run report JSON to this path.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Write a redacted JUnit XML report to this path.
    #[arg(long, value_name = "PATH")]
    pub junit: Option<PathBuf>,

    /// Emit a single-line `deja-summary v=1` trailer.
    #[arg(long)]
    pub summary: bool,

    /// Hidden trap so `deja remember --verify` emits a clear exit-2 usage message.
    #[arg(long, hide = true)]
    pub verify: bool,
}

/// Arguments for `deja list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Isolated store identifier under `.deja/memory/stores/<id>/`.
    #[arg(long, value_name = "ID")]
    pub store: Option<String>,
}

/// Arguments for `deja inspect <target>`.
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Path to a saved JSON report file OR a goal name stored in SQLite.
    #[arg(value_name = "TARGET")]
    pub target: String,

    /// Always resolve TARGET as a goal name stored in SQLite (even if it looks like a path or exists on disk).
    #[arg(long)]
    pub goal: bool,

    /// Isolated store identifier under `.deja/memory/stores/<id>/`.
    #[arg(long, value_name = "ID")]
    pub store: Option<String>,

    /// Emit a single-line `deja-summary v=1` trailer when inspecting a run report.
    #[arg(long)]
    pub summary: bool,
}

/// Arguments for `deja manifest <subcommand>`.
#[derive(Debug, Args)]
pub struct ManifestArgs {
    #[command(subcommand)]
    pub command: ManifestCommand,
}

#[derive(Debug, Subcommand)]
pub enum ManifestCommand {
    /// Check that consumer flow files on disk match `deja.walks.json` without a device or store.
    Check(ManifestCheckArgs),
}

#[derive(Debug, Args)]
pub struct ManifestCheckArgs {
    /// Also list `.yaml`/`.yml` files under `flowsDir` and manifest flow folders not referenced by any entry.
    #[arg(long)]
    pub untracked: bool,
}

/// Arguments for `deja gate`.
#[derive(Debug, Args)]
pub struct GateArgs {
    /// Required goals file (plain lines, JSON array, or `deja.walks.json`).
    #[arg(long, required = true, value_name = "FILE")]
    pub goals: PathBuf,

    /// Build file or `.app` directory to fingerprint (defaults to `config.buildPath`).
    #[arg(long, value_name = "PATH")]
    pub build: Option<PathBuf>,

    /// Opaque release label recorded in evidence (for example a git commit SHA).
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,

    /// Evidence JSON output path (defaults to `.deja/gate/evidence.json`).
    #[arg(long, visible_alias = "out", value_name = "FILE")]
    pub evidence: Option<PathBuf>,

    /// Store id(s) to evaluate (repeatable for multi-device shard gates).
    #[arg(long = "store", value_name = "ID")]
    pub stores: Vec<String>,

    /// Optional runtime isolation filter (for example `ios-27.2`).
    #[arg(long, value_name = "VERSION")]
    pub runtime: Option<String>,
}

/// Arguments for `deja doctor`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Optional target device / simulator UDID to verify (read-only, never boots).
    #[arg(long, value_name = "ID")]
    pub device: Option<String>,
}

/// Arguments for `deja migrate`.
#[derive(Debug, Args)]
pub struct MigrateArgs {
    /// Preview migration actions without writing SQLite.
    #[arg(long)]
    pub dry_run: bool,

    /// Isolated store identifier under `.deja/memory/stores/<id>/`.
    #[arg(long, value_name = "ID")]
    pub store: Option<String>,
}

/// Arguments for `deja engine <subcommand>`.
#[derive(Debug, Args)]
pub struct EngineArgs {
    #[command(subcommand)]
    pub command: EngineCommand,
}

#[derive(Debug, Subcommand)]
pub enum EngineCommand {
    /// Retired Maestro installer subcommand (exits 2 with native-runner notice).
    Install,
}

/// Arguments for `deja mcp`.
#[derive(Debug, Args)]
pub struct McpArgs {
    /// Isolated store identifier under `.deja/memory/stores/<id>/`.
    #[arg(long, value_name = "ID")]
    pub store: Option<String>,

    /// Override target device / simulator UDID (`--device` > `DEJA_DEVICE` > `config.device`).
    #[arg(long, value_name = "ID")]
    pub device: Option<String>,
}

/// Arguments for `deja daemon <subcommand>`.
#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

/// Subcommands under `deja daemon`.
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start the persistent warm driver daemon (idempotent if already running).
    Start(DaemonStartArgs),

    /// Inspect daemon liveness, idle TTL, and warm pooled sessions.
    Status,

    /// Gracefully stop the warm driver daemon and shut down all pooled drivers.
    Stop,
}

/// Arguments for `deja daemon start`.
#[derive(Debug, Args)]
pub struct DaemonStartArgs {
    /// Run the daemon in the foreground until SIGINT, SIGTERM, `deja daemon stop`, or idle TTL.
    #[arg(long)]
    pub foreground: bool,

    /// Shut down automatically after this many idle seconds with zero active leases.
    #[arg(long, value_name = "SECS", default_value_t = crate::daemon::DEFAULT_IDLE_TIMEOUT_SECS)]
    pub idle_timeout: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_parses_dash_goal_after_double_dash() {
        let cli = Cli::try_parse_from(["deja", "run", "--", "-dash-goal"]).unwrap();
        match cli.command {
            Some(Commands::Run(args)) => assert_eq!(args.goal, vec!["-dash-goal"]),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_batch_flags_and_devices_delimiter() {
        let cli = Cli::try_parse_from([
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--devices",
            "sim-1,sim-2",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Run(args)) => {
                assert!(args.goal.is_empty());
                assert_eq!(args.goals, Some(PathBuf::from("goals.txt")));
                assert!(!args.all);
                assert_eq!(args.devices, vec!["sim-1", "sim-2"]);
            }
            other => panic!("expected Run, got {other:?}"),
        }

        let cli_all = Cli::try_parse_from(["deja", "run", "--all"]).unwrap();
        match cli_all.command {
            Some(Commands::Run(args)) => {
                assert!(args.goal.is_empty());
                assert!(args.goals.is_none());
                assert!(args.all);
                assert!(args.devices.is_empty());
            }
            other => panic!("expected Run, got {other:?}"),
        }

        // Empty `--devices ""` and `"d1,,d2"` preserve empty tokens for fail-closed validation.
        let cli_empty_dev = Cli::try_parse_from(["deja", "run", "--all", "--devices", ""]).unwrap();
        match cli_empty_dev.command {
            Some(Commands::Run(args)) => assert_eq!(args.devices, vec![""]),
            other => panic!("expected Run, got {other:?}"),
        }

        let cli_gap_dev =
            Cli::try_parse_from(["deja", "run", "--all", "--devices", "d1,,d2"]).unwrap();
        match cli_gap_dev.command {
            Some(Commands::Run(args)) => assert_eq!(args.devices, vec!["d1", "", "d2"]),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn gate_parses_repeatable_store_and_out_alias() {
        let cli = Cli::try_parse_from([
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            "sim-a",
            "--store",
            "sim-b",
            "--out",
            "out/gate.json",
            "--runtime",
            "ios-27.2",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Gate(args)) => {
                assert_eq!(args.stores, vec!["sim-a", "sim-b"]);
                assert_eq!(args.evidence, Some(PathBuf::from("out/gate.json")));
                assert_eq!(args.runtime.as_deref(), Some("ios-27.2"));
            }
            other => panic!("expected Gate, got {other:?}"),
        }
    }

    #[test]
    fn daemon_subcommands_and_global_no_daemon_parse() {
        let start = Cli::try_parse_from([
            "deja",
            "daemon",
            "start",
            "--foreground",
            "--idle-timeout",
            "45",
        ])
        .unwrap();
        match start.command {
            Some(Commands::Daemon(DaemonArgs {
                command: DaemonCommand::Start(args),
            })) => {
                assert!(args.foreground);
                assert_eq!(args.idle_timeout, 45);
            }
            other => panic!("expected Daemon::Start, got {other:?}"),
        }

        let status = Cli::try_parse_from(["deja", "daemon", "status", "--json"]).unwrap();
        assert!(status.json);
        assert!(matches!(
            status.command,
            Some(Commands::Daemon(DaemonArgs {
                command: DaemonCommand::Status
            }))
        ));

        let stop = Cli::try_parse_from(["deja", "daemon", "stop"]).unwrap();
        assert!(matches!(
            stop.command,
            Some(Commands::Daemon(DaemonArgs {
                command: DaemonCommand::Stop
            }))
        ));

        let bypass = Cli::try_parse_from(["deja", "--no-daemon", "run", "checkout"]).unwrap();
        assert!(bypass.no_daemon);
        assert!(matches!(bypass.command, Some(Commands::Run(_))));
    }
}
