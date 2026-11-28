//! `deja mcp` — launch the stdio JSON-RPC 2.0 MCP server confined to the configured workspace,
//! store, platform, appId, buildPath, and device.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use deja_flow::{DriverError, PlatformDriver};
use deja_mcp::{McpBoxFuture, McpDriverContext, McpDriverFactory, McpServerConfig, McpServerError};
use deja_store::validate_store_id;
use tokio_util::sync::CancellationToken;

use crate::CliDeps;
use crate::cli::McpArgs;
use crate::config::{CommandCapability, LoadConfigOptions, load_config};
use crate::driver_factory::{DriverContext, DriverFactory, DriverFactoryError};
use crate::exit_codes::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE_OR_CONFIG};
use crate::output::RedactionContext;

/// Bridges `deja-cli`'s concrete [`DriverFactory`] into `deja-mcp`'s trait boundary so
/// `deja-mcp` never links concrete platform drivers or `deja-cli`.
struct CliMcpDriverFactory {
    inner: Arc<dyn DriverFactory>,
    deja_home: Option<PathBuf>,
    server_cancel: CancellationToken,
}

impl CliMcpDriverFactory {
    fn to_driver_context(&self, ctx: &McpDriverContext) -> DriverContext {
        DriverContext {
            platform: ctx.platform.clone(),
            device: ctx.device.clone(),
            app_id: ctx.app_id.clone(),
            build_path: ctx.build_path.clone(),
            expected_build_fingerprint: ctx.expected_build_fingerprint.clone(),
            workspace_root: ctx.workspace_root.clone(),
            disable_animations: ctx.disable_animations,
            crash_report_grace_ms: ctx.crash_report_grace_ms,
            crash_report_dir: ctx.crash_report_dir.clone(),
            deja_home: self.deja_home.clone(),
            headless: ctx.headless,
            screen_size: ctx.screen_size.clone(),
            timeout_ms: ctx.timeout_ms,
            env: ctx.env.clone(),
        }
    }
}

fn map_factory_error(operation: &str, err: DriverFactoryError) -> DriverError {
    match err {
        DriverFactoryError::Cancelled => DriverError::Cancelled,
        DriverFactoryError::UnsupportedOrConfig(msg) => DriverError::InvalidArgument(msg),
        DriverFactoryError::Runtime(msg) => DriverError::Transport {
            operation: operation.to_string(),
            detail: msg,
            transient: false,
        },
    }
}

impl McpDriverFactory for CliMcpDriverFactory {
    fn resolve_runtime<'a>(
        &'a self,
        ctx: &'a McpDriverContext,
        cancel: &'a CancellationToken,
    ) -> McpBoxFuture<'a, Result<Option<String>, DriverError>> {
        let driver_ctx = self.to_driver_context(ctx);
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            inner
                .resolve_runtime(&driver_ctx, cancel)
                .await
                .map_err(|err| map_factory_error("resolve_runtime", err))
        })
    }

    fn install_app<'a>(
        &'a self,
        ctx: &'a McpDriverContext,
        cancel: &'a CancellationToken,
    ) -> McpBoxFuture<'a, Result<(), DriverError>> {
        let driver_ctx = self.to_driver_context(ctx);
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            inner
                .install_app(&driver_ctx, cancel)
                .await
                .map_err(|err| map_factory_error("install_app", err))
        })
    }

    fn create_driver<'a>(
        &'a self,
        ctx: &'a McpDriverContext,
    ) -> McpBoxFuture<'a, Result<Box<dyn PlatformDriver>, DriverError>> {
        let driver_ctx = self.to_driver_context(ctx);
        let inner = Arc::clone(&self.inner);
        let cancel = self.server_cancel.child_token();
        Box::pin(async move {
            inner
                .create(&driver_ctx, &cancel)
                .await
                .map_err(|err| map_factory_error("create_driver", err))
        })
    }
}

pub async fn run(
    args: &McpArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    if json_mode {
        deps.output.writeln_stderr(
            "deja mcp error: `--json` is not valid with `deja mcp` (stdout is reserved for the MCP JSON-RPC 2.0 stream)",
        );
        return EXIT_USAGE_OR_CONFIG;
    }

    if let Some(ref store_id) = args.store
        && validate_store_id(store_id).is_err()
    {
        deps.output.writeln_stderr(
            "deja mcp error: invalid `--store` value (must be 1..=64 characters matching `[a-z0-9._-]`, starting with `[a-z0-9]`, without path separators or `..`)",
        );
        return EXIT_USAGE_OR_CONFIG;
    }

    let config = match load_config(&LoadConfigOptions {
        cwd: &deps.cwd,
        explicit_config,
        capability: CommandCapability::Doctor,
        device_override: args.device.as_deref(),
        build_override: None,
        env: &deps.env,
    }) {
        Ok(cfg) => cfg,
        Err(err) => {
            let redaction =
                RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);
            deps.output.writeln_stderr(&format!(
                "deja mcp error: {}",
                redaction.redact_str(&err.to_string())
            ));
            return EXIT_USAGE_OR_CONFIG;
        }
    };

    for warning in &config.warnings {
        deps.output.writeln_stderr(warning);
    }

    let redaction = RedactionContext::new(
        &config.workspace_root,
        deps.home_dir.as_deref(),
        &config.forbidden_values,
        &deps.env,
    );

    let server_cfg = McpServerConfig {
        workspace_root: config.workspace_root.clone(),
        canonical_workspace_root: config.workspace_root.clone(),
        home_dir: deps.home_dir.clone(),
        store_id: args.store.clone(),
        platform: config.platform.clone(),
        app_id: config.app_id.clone(),
        adapter: config.adapter.clone(),
        browser: config.browser.clone(),
        build_path: config.build_path.clone(),
        device: config.device.clone(),
        manifest_path: config.manifest.clone(),
        flows_dir: config.flows_dir.clone(),
        timeout_ms: config.timeout_ms,
        verification_timeout_ms: config.verification_timeout_ms,
        retries: config.retries,
        crash_report_grace_ms: config.crash_report_grace_ms,
        crash_report_dir: config.crash_report_directory.clone(),
        disable_animations: config.disable_animations,
        stale_build: config.stale_build,
        env: config.env.clone(),
        process_env: deps.env.clone(),
        forbidden_values: config.forbidden_values.clone(),
        mcp_explore: config.mcp.as_ref().is_some_and(|m| m.explore),
        explore_backend_available: false,
        headless: config.headless,
        screen_size: config.screen_size.clone(),
    };

    let deja_home = deps
        .env
        .get("DEJA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let factory: Arc<dyn McpDriverFactory> = Arc::new(CliMcpDriverFactory {
        inner: Arc::clone(&deps.driver_factory),
        deja_home,
        server_cancel: deps.cancel.clone(),
    });

    match deja_mcp::serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        server_cfg,
        factory,
        deps.cancel.clone(),
    )
    .await
    {
        Ok(()) => EXIT_OK,
        Err(McpServerError::InvalidConfig(msg)) => {
            deps.output
                .writeln_stderr(&format!("deja mcp error: {}", redaction.redact_str(&msg)));
            EXIT_USAGE_OR_CONFIG
        }
        Err(McpServerError::Io(err)) => {
            deps.output.writeln_stderr(&format!(
                "deja mcp error: {}",
                redaction.redact_str(&err.to_string())
            ));
            EXIT_FAILURE
        }
    }
}
