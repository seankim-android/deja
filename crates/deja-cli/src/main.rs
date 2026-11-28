use clap::{Parser, Subcommand};

const LINKED_WORKSPACE_CRATES: [&str; 8] = [
    deja_core::CRATE_NAME,
    deja_store::CRATE_NAME,
    deja_flow::CRATE_NAME,
    deja_driver_ios::CRATE_NAME,
    deja_driver_android::CRATE_NAME,
    deja_driver_web::CRATE_NAME,
    deja_explore::CRATE_NAME,
    deja_mcp::CRATE_NAME,
];

/// Embedded Déjà agent workflow guide printed by `deja guide`.
pub const EMBEDDED_AGENT_GUIDE_MD: &str = include_str!("../../../docs/AGENT_GUIDE.md");

#[derive(Debug, Parser)]
#[command(
    name = "deja",
    version,
    about = "Verified app test memory: explore once, replay with zero model calls"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print the embedded Déjà agent workflow guide (`docs/AGENT_GUIDE.md`).
    Guide,
}

fn main() {
    let cli = Cli::parse();
    let _ = LINKED_WORKSPACE_CRATES;
    if let Some(Commands::Guide) = cli.command {
        print!("{EMBEDDED_AGENT_GUIDE_MD}");
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, EMBEDDED_AGENT_GUIDE_MD, LINKED_WORKSPACE_CRATES};
    use clap::CommandFactory;

    #[test]
    fn verify_cli_definition() {
        Cli::command().debug_assert();
    }

    #[test]
    fn composition_root_links_all_workspace_crates() {
        assert_eq!(LINKED_WORKSPACE_CRATES.len(), 8);
    }

    #[test]
    fn embedded_agent_guide_is_present_and_well_formed() {
        assert!(EMBEDDED_AGENT_GUIDE_MD.starts_with("# Déjà Agent Guide\n"));
        assert!(EMBEDDED_AGENT_GUIDE_MD.contains("deja doctor"));
        assert!(EMBEDDED_AGENT_GUIDE_MD.contains("deja remember"));
        assert!(EMBEDDED_AGENT_GUIDE_MD.contains("deja run"));
    }
}
