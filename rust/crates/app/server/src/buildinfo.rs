//! The running server's build identity, captured once at startup.

use std::path::Path;

use charliebot_cli_common::skew::{SHA_FIELD, STARTED_AT_FIELD};
use charliebot_core::{git, timefmt};
use serde_json::{Value, json};

/// The git sha and UTC start time `GET /api/internal/version` reports.
#[derive(Debug, Clone)]
pub struct BuildInfo {
  /// `git rev-parse --short HEAD` in the repository root, or `"unknown"` when git fails: the CLI's
  /// version-skew hint reads this field and treats `"unknown"` as "cannot tell".
  pub sha: String,
  /// The UTC start time in ISO 8601, at capture.
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

  /// The version response's object: the sha and start time under the field names `skew` owns, in that order.
  pub fn to_json(&self) -> Value {
    json!({ (SHA_FIELD): self.sha, (STARTED_AT_FIELD): self.started_at })
  }
}
