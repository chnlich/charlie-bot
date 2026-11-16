//! The CLI's own timeout bounds: one internal-API call, the version probe, and the connect-retry budget.
//! `git::SHA_TIMEOUT` bounds the git SHA read that the version-skew hint makes.

use std::time::Duration;

/// One internal-API request: local loopback, generous for cold starts.
pub const HTTP_INTERNAL_API_TIMEOUT: Duration = Duration::from_secs(30);

/// Best-effort fetch of `/api/internal/version` on the CLI error path, bounded so a hung
/// server never stalls CLI error reporting.
pub const HTTP_VERSION_SKEW_TIMEOUT: Duration = Duration::from_secs(2);

/// Total budget for a CLI to retry a request whose connection never got established
/// (server down or restarting). Exponential backoff runs inside it.
pub const CLI_CONNECT_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
