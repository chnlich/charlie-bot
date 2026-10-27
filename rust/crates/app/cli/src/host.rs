//! The composed host of the CLI processes that run a backend (`charliebot_agents::cli_host`): the packages the server
//! registers, so the config loads with every section a real `config.yaml` carries and every backend type exists.
//!
//! The lists mirror `crates/app/server/src/packages/mod.rs`, which the server binary owns. A package the server adds
//! there belongs here too, or a verb that loads the config refuses that package's section.

use std::sync::Arc;

use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::cli_host::CliHost;
use charliebot_core::home;
use charliebot_core::repo_root::{REPO_ROOT_ENV, RepoRoot};
use charliebot_registry::{RegisterFn, Registry};

const RUNTIME: &[(&str, RegisterFn)] = &[
  ("runtime.agents", charliebot_agents::register_runtime),
  ("runtime.usage", charliebot_usage::register),
  ("runtime.session_api", charliebot_session_api::register),
  ("runtime.tasks", charliebot_tasks::register),
  ("runtime.triggers", charliebot_triggers::register),
];

const PACKAGES: &[(&str, RegisterFn)] = &[
  ("src.features.artifacts", charliebot_artifacts::register),
  ("src.features.backup", charliebot_backup::register),
  ("src.features.backend_pool", charliebot_agents::backend_pool::register),
  ("src.features.chat_threads", charliebot_chat_threads::register),
  ("src.features.code_server", charliebot_code_server::register),
  ("src.features.cron", charliebot_cron::register),
  ("src.features.diag", charliebot_diag::register),
  ("src.features.slack", charliebot_slack::register),
  ("src.features.discord", charliebot_discord::register),
  ("src.features.disk_sweep", charliebot_disk_sweep::register),
  ("src.features.web_reply", charliebot_web_reply::register),
  ("src.features.host_auth", charliebot_host_auth::register),
  ("src.features.diff_view", charliebot_diff_view::register),
  ("src.features.explain", charliebot_explain::register),
  ("src.features.improve", charliebot_improve::register),
  ("src.features.memory", charliebot_memory::register),
  ("src.features.ncu", charliebot_ncu::register),
  ("src.features.terminal", charliebot_terminal::register),
  ("src.features.trace", charliebot_trace::register),
  ("src.features.usage", charliebot_usage_page::register),
  ("src.features.voice", charliebot_voice::register),
  ("src.backends.openai_compatible", charliebot_agents::backends::openai_compatible::register),
  ("src.backends.antigravity", charliebot_agents::backends::antigravity::register),
  ("src.backends.charlie_code", charliebot_agents::backends::charlie_code::register),
  ("src.backends.claude_code", charliebot_agents::backends::claude_code::register),
  ("src.backends.codex", charliebot_agents::backends::codex::register),
  ("src.backends.gemini", charliebot_agents::backends::gemini::register),
  ("src.backends.anthropic_compatible", charliebot_agents::backends::anthropic_compatible::register),
  ("src.backends.opencode", charliebot_agents::backends::opencode::register),
  ("src.features.files", charliebot_files::register),
];

/// The `CliHostFactory` of this binary: every package registered in server order, the config loaded against that
/// registry, and the process environment as the agent base environment.
pub fn compose() -> anyhow::Result<CliHost> {
  let mut registry = Registry::new();
  for &(package, register) in RUNTIME.iter().chain(PACKAGES.iter()) {
    registry.register_package(package, register)?;
  }
  let cfg = registry.load_config(&home::charliebot_home_dir()?)?;
  let repo_root = RepoRoot::new(std::env::var_os(REPO_ROOT_ENV))?.path().to_path_buf();
  Ok(CliHost {
    cfg,
    registry: Arc::new(registry),
    repo_root,
    base_env: AgentBaseEnvironment { values: std::env::vars().collect() },
    pool: BackendPool::default(),
  })
}
