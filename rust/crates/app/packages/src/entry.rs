//! The server's package list: the runtime registrations, the listed packages in registration order, and the boot-time
//! cron seeding that the list owns.

use std::path::Path;

use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::{RegisterFn, Registry, RegistryError};

use crate::ui_text_config;

/// Runtime registrations run before the package list, matching the server's pre-recovery ordering.
const RUNTIME: &[(&str, RegisterFn)] = &[
  ("runtime.agents", charliebot_agents::register_runtime),
  ("runtime.usage", charliebot_usage::register),
  ("runtime.session_api", charliebot_session_api::register),
  ("runtime.tasks", charliebot_tasks::register),
  ("runtime.triggers", charliebot_triggers::register),
];

/// The listed packages in registration order.
const PACKAGES: &[(&str, RegisterFn)] = &[
  ("features.user_text_checks", charliebot_user_text_checks::register),
  (charliebot_artifacts::PACKAGE, charliebot_artifacts::register),
  ("features.backup", charliebot_backup::register),
  (charliebot_agents::backend_pool::PACKAGE, charliebot_agents::backend_pool::register),
  ("features.chat_threads", charliebot_chat_threads::register),
  ("features.code_server", charliebot_code_server::register),
  ("features.cron", charliebot_cron::register),
  ("features.diag", charliebot_diag::register),
  ("features.drafts", charliebot_drafts::register),
  ("features.slack", charliebot_slack::register),
  ("features.github", charliebot_github::register),
  ("features.discord", charliebot_discord::register),
  ("features.feishu", charliebot_feishu::register),
  ("features.disk_sweep", charliebot_disk_sweep::register),
  ("features.run_log_compaction", charliebot_run_log_compaction::register),
  ("features.web_reply", charliebot_web_reply::register),
  ("features.host_auth", charliebot_host_auth::register),
  ("features.diff_view", charliebot_diff_view::register),
  ("features.explain", charliebot_explain::register),
  ("features.failure_triage", charliebot_failure_triage::register),
  ("features.improve", charliebot_improve::register),
  ("features.memory", charliebot_memory::register),
  ("features.taste", charliebot_taste::register),
  ("features.terminal", charliebot_terminal::register),
  ("features.trace", charliebot_trace::register),
  ("features.usage", charliebot_usage_page::register),
  ("features.voice", charliebot_voice::register),
  ("features.web_search", charliebot_web_search::register),
  (charliebot_openai_compatible::PACKAGE, charliebot_openai_compatible::register),
  (charliebot_antigravity::PACKAGE, charliebot_antigravity::register),
  (charliebot_charlie_code::PACKAGE, charliebot_charlie_code::register),
  (charliebot_claude_code::PACKAGE, ui_text_config::register_claude_code),
  (charliebot_codex::PACKAGE, charliebot_codex::register),
  (charliebot_devin::PACKAGE, charliebot_devin::register),
  (charliebot_gemini::PACKAGE, charliebot_gemini::register),
  (charliebot_anthropic_compatible::PACKAGE, charliebot_anthropic_compatible::register),
  (charliebot_opencode::PACKAGE, charliebot_opencode::register),
  ("features.files", charliebot_files::register),
];

/// The registry of every listed package, registered in list order. A failing registration fails the server's
/// startup.
pub fn registry() -> Result<Registry, RegistryError> {
  let mut registry = Registry::new();
  for &(package, register) in RUNTIME {
    registry.register_package(package, register)?;
  }
  for &(package, register) in PACKAGES {
    registry.register_package(package, register)?;
  }
  Ok(registry)
}

/// The boot-time seeding the package list owns: the cron package writes the repo's default cron tasks into
/// `config.d/cron.d/` on every server start, before the store opens. The server's startup calls this after
/// `init_charliebot_home`; a package that must seed before the store opens adds its call here.
pub fn seed_default_cron_tasks(
  cfg: &CharlieBotConfig,
  repo_root: &Path,
) -> Result<Vec<charliebot_cron::seed::SeedReport>, String> {
  charliebot_cron::seed::seed_default_cron_tasks(cfg, repo_root, false)
}
