//! The opencode timeouts and snapshot context reserve (`src/backends/opencode/opencode_limits.py`).

use std::time::Duration;

use charliebot_registry::backend::ContextLimits;
use charliebot_store::run_records::RunKind;

pub const OPENCODE_HTTP_API_TIMEOUT: Duration = Duration::from_secs(30);
pub const OPENCODE_SERVER_START_TIMEOUT: Duration = Duration::from_secs(30);
pub const OPENCODE_SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(5);
pub const OPENCODE_STREAM_EXIT_GRACE: Duration = Duration::from_millis(200);
pub const OPENCODE_SERVER_IDLE_EXIT_SECONDS: Duration = Duration::from_secs(3600);
pub const OPENCODE_SERVER_HEAVY_CHECK_SECONDS: Duration = Duration::from_secs(60);
pub const OPENCODE_SERVER_HEAVY_RSS_MB: f64 = 400.0;
pub const OPENCODE_INTERRUPT_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const OPENCODE_SSE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(3600);
pub const OPENCODE_COMPACT_OUTPUT_RESERVE: u64 = 20_000;

/// The context limits of one snapshot reading.
pub fn snapshot_reading_limits(_run_kind: RunKind) -> ContextLimits {
  ContextLimits { declared_window: None, compact_reserve: Some(OPENCODE_COMPACT_OUTPUT_RESERVE) }
}
