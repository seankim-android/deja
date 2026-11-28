//! `deja guide` — print the embedded Déjà agent workflow guide verbatim.

use crate::CliDeps;
use crate::exit_codes::EXIT_OK;

/// Embedded Déjà agent workflow guide (`docs/AGENT_GUIDE.md`).
pub const EMBEDDED_AGENT_GUIDE_MD: &str = include_str!("../../../../docs/AGENT_GUIDE.md");

pub fn run(deps: &CliDeps) -> u8 {
    deps.output.write_stdout(EMBEDDED_AGENT_GUIDE_MD);
    EXIT_OK
}
