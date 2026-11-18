//! The codex backend's process environment: the CLI host every backend type reads, and the codex home under it.

use std::path::PathBuf;

pub use charliebot_agents::cli_base::CliHost as CodexHost;

/// `~/.codex` of `host`: the codex home that holds the rollouts. The user-local bin directory is
/// [`charliebot_agents::cli_base::user_local_bin`] of the same home.
pub fn codex_home(host: &CodexHost) -> PathBuf {
  host.home.join(".codex")
}
