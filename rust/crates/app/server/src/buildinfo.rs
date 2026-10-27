//! The running server's build identity, captured once at startup (`src/infra/buildinfo.py`).

use std::path::Path;

use charliebot_core::{git, timefmt};
use serde_json::{Value, json};

/// The git sha and UTC start time `GET /api/internal/version` reports.
#[derive(Debug, Clone)]
pub struct BuildInfo {
  /// `git rev-parse --short HEAD` in the repository root, or `"unknown"` when git fails: the CLI's
  /// version-skew hint reads this field and treats `"unknown"` as "cannot tell".
  pub sha: String,
  /// `datetime.now(UTC).isoformat()` at capture.
  pub started_at: String,
}

impl BuildInfo {
  /// Capture the sha and the start time now.
  pub fn capture(repo_root: &Path) -> BuildInfo {
    BuildInfo {
      sha: git::head_sha(repo_root, git::SHA_TIMEOUT).unwrap_or_else(|| "unknown".to_string()),
      started_at: timefmt::utc_now_iso(),
    }
  }

  /// `{"sha": ..., "started_at": ...}`, in that key order.
  pub fn to_json(&self) -> Value {
    json!({"sha": self.sha, "started_at": self.started_at})
  }
}
