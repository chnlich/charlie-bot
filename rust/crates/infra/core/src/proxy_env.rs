//! The process-environment names of an outbound proxy.
//!
//! An outbound client reads `HTTPS_PROXY` and `NO_PROXY` from the process environment; a spawned backend CLI inherits
//! all three names, with `HTTP_PROXY` and `HTTPS_PROXY` carrying the same URL. The names describe one concept, so they
//! are defined here and the client and the CLI environment reference them.

/// The variable holding the proxy URL for HTTP requests.
pub const HTTP_PROXY: &str = "HTTP_PROXY";
/// The variable holding the proxy URL for HTTPS requests.
pub const HTTPS_PROXY: &str = "HTTPS_PROXY";
/// The variable holding the hosts that bypass the proxy.
pub const NO_PROXY: &str = "NO_PROXY";
