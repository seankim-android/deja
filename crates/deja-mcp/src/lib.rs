//! Stdio JSON-RPC 2.0 MCP server for Déjà with cancellation, workspace/app confinement,
//! warm-driver & build-fingerprint reuse, pruned visible-element diagnostics (`DEJA-83`),
//! and batch step execution (`DEJA-84`).

pub mod backend;
pub mod fs_safe;
pub mod protocol;
pub mod redaction;
pub mod server;
pub mod tools;

pub use backend::{
    McpBoxFuture, McpDriverContext, McpDriverFactory, McpServerConfig, McpServerError,
};
pub use protocol::{FramingMode, MAX_FRAME_BYTES};
pub use server::{
    DEFAULT_WRITE_BACKPRESSURE_TIMEOUT, MAX_IN_FLIGHT_TOOL_CALLS, McpServerLimits, serve,
    serve_with_limits,
};

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-mcp";

/// Direct upstream workspace crates linked by `deja-mcp`.
pub const UPSTREAM_CRATES: &[&str] = &[
    deja_core::CRATE_NAME,
    deja_flow::CRATE_NAME,
    deja_store::CRATE_NAME,
    deja_explore::CRATE_NAME,
];

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-mcp");
        assert_eq!(
            UPSTREAM_CRATES,
            &["deja-core", "deja-flow", "deja-store", "deja-explore"]
        );
    }
}
