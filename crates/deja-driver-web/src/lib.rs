//! Native Web driver using direct Chromium Chrome DevTools Protocol (CDP) WebSockets.

pub mod browser;
pub mod cdp;
pub mod dom;
pub mod driver;
pub mod error;
pub mod input;

pub use browser::{discover_chromium, probe_chromium_version};
pub use driver::{WebDriver, WebDriverConfig};
pub use error::{Result, WebDriverError, map_web_err};
pub use input::{ScreenSize, validate_loopback_http_url};

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-driver-web";

/// Direct upstream workspace crates linked by `deja-driver-web`.
pub const UPSTREAM_CRATES: &[&str] = &[deja_core::CRATE_NAME, deja_flow::CRATE_NAME];

#[cfg(test)]
mod tests;
