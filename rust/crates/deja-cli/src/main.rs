use clap::Parser;

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

#[derive(Debug, Parser)]
#[command(
    name = "deja",
    version,
    about = "Verified app test memory: explore once, replay with zero model calls"
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    let _ = LINKED_WORKSPACE_CRATES;
}

#[cfg(test)]
mod tests {
    use super::{Cli, LINKED_WORKSPACE_CRATES};
    use clap::CommandFactory;

    #[test]
    fn verify_cli_definition() {
        Cli::command().debug_assert();
    }

    #[test]
    fn composition_root_links_all_workspace_crates() {
        assert_eq!(LINKED_WORKSPACE_CRATES.len(), 8);
    }
}
