//! The composition root both binaries share: the runtime registrations and the server's package list, in the
//! server's order.
//!
//! Each entry names a package by its `features.*` or `backends.*` label and the `register` function that makes the package's
//! contributions. This crate owns the list: a package added to the server is added here. The list keeps every
//! package, so the page parts, the static mounts and the template search path keep their registration order.
//!
//! One crate owns both lists, so the server binary and the CLI binary cannot drift: a package added here reaches
//! both.

use charliebot_registry::{RegisterFn, Registry, RegistryError};

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
  ("features.artifacts", charliebot_artifacts::register),
  ("features.backup", charliebot_backup::register),
  ("features.backend_pool", charliebot_agents::backend_pool::register),
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
  (charliebot_claude_code::PACKAGE, charliebot_claude_code::register),
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
