//! The CLI's own timeout bounds: one internal-API call, the version probe, the git SHA read, and the
//! connect-retry budget.

use std::time::Duration;

/// One internal-API request: local loopback, generous for cold starts.
pub const HTTP_INTERNAL_API_TIMEOUT: Duration = Duration::from_secs(30);

/// Best-effort fetch of `/api/internal/version` on the CLI error path, bounded so a hung
/// server never stalls CLI error reporting.
pub const HTTP_VERSION_SKEW_TIMEOUT: Duration = Duration::from_secs(2);

/// Best-effort `git rev-parse --short HEAD` read behind the version-skew hint.
pub const SUBPROCESS_GIT_SHA_TIMEOUT: Duration = Duration::from_secs(2);

/// Total budget for a CLI to retry a request whose connection never got established
/// (server down or restarting); exponential backoff inside.
pub const CLI_CONNECT_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
