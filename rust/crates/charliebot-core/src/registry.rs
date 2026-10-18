//! What the registered packages contribute, as data: the Rust side of `src/app/registrations.py`.
//!
//! The Python server fills its registries by calling each listed package's `register()`. The Rust
//! server reads the same contributions from the tables below; a port stage that brings a package's
//! behavior over adds that package's rows here.

use std::path::{Path, PathBuf};

use crate::metadata_slot::MetadataSlot;

/// Runtime contributions whose order follows package registration.
#[derive(Debug, Default)]
pub struct Registry {
  metadata_slots: Vec<MetadataSlot>,
}

impl Registry {
  /// Register one metadata slot in package registration order.
  pub fn register_metadata_slot(&mut self, slot: MetadataSlot) -> Result<(), String> {
    if self.metadata_slots.iter().any(|registered| registered.owner == slot.owner && registered.on == slot.on) {
      return Err(format!("metadata owner {:?} is already registered for {:?}", slot.owner, slot.on));
    }
    self.metadata_slots.push(slot);
    Ok(())
  }

  /// Return the collected metadata slots in registration order.
  pub fn metadata_slots(&self) -> &[MetadataSlot] {
    &self.metadata_slots
  }
}

/// `registrations.PACKAGES`, in registration order. Every listed package is a page package: its
/// `static/` serves under `/static/<kind>/<name>/` and its `templates/` joins the template search
/// path, in this order (`page_render.register_page_package`).
pub const PACKAGES: &[&str] = &[
  "src.features.artifacts",
  "src.features.backup",
  "src.features.backend_pool",
  "src.features.chat_threads",
  "src.features.code_server",
  "src.features.cron",
  "src.features.diag",
  "src.features.slack",
  "src.features.discord",
  "src.features.web_reply",
  "src.features.host_auth",
  "src.features.diff_view",
  "src.features.explain",
  "src.features.improve",
  "src.features.memory",
  "src.features.ncu",
  "src.features.terminal",
  "src.features.trace",
  "src.features.usage",
  "src.features.voice",
  "src.backends.openai_compatible",
  "src.backends.antigravity",
  "src.backends.charlie_code",
  "src.backends.claude_code",
  "src.backends.codex",
  "src.backends.gemini",
  "src.backends.kimi",
  "src.backends.opencode",
  "src.features.files",
];

/// The traits of one backend type that the ported stages read (`BackendTraits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendType {
  /// The `backends.options[].type` string.
  pub name: &'static str,
  /// Whether the backend runs a `/compact` chat message as a manual compaction.
  pub manual_compact: bool,
  /// The compact button's hint, or `None`.
  pub compact_hint: Option<&'static str>,
}

/// The registered backend types, in registration order (`backend_type_registration.register_backend_type`).
pub const BACKEND_TYPES: &[BackendType] = &[
  BackendType { name: "cc-openai-compatible", manual_compact: false, compact_hint: None },
  BackendType { name: "antigravity", manual_compact: false, compact_hint: None },
  BackendType { name: "charlie-code", manual_compact: false, compact_hint: None },
  BackendType { name: "cc-claude", manual_compact: true, compact_hint: None },
  BackendType {
    name: "codex",
    manual_compact: false,
    compact_hint: Some("codex only compacts automatically — tune model_auto_compact_token_limit"),
  },
  BackendType { name: "gemini", manual_compact: false, compact_hint: None },
  BackendType { name: "cc-kimi", manual_compact: false, compact_hint: None },
  BackendType { name: "opencode", manual_compact: false, compact_hint: None },
];

/// The registered type named `name`, or `None` when no package registered it.
pub fn backend_type(name: &str) -> Option<&'static BackendType> {
  BACKEND_TYPES.iter().find(|backend| backend.name == name)
}

/// The top-level config sections packages own, in registration order (`config_registry.register_config_section`).
/// Each section stays raw YAML until its package's port stage types it.
pub const CONFIG_SECTIONS: &[&str] = &["publish", "code_server", "slack", "discord", "voice", "accounts"];

/// A legacy-key value under this prefix names a `credentials.yaml` key path (`config_registry.CREDENTIALS_PREFIX`).
pub const CREDENTIALS_PREFIX: &str = "credentials: ";

/// The retired top-level config keys and where each moved: `config.LEGACY_KEYS` followed by the keys
/// packages registered, in registration order.
pub const LEGACY_KEYS: &[(&str, &str)] = &[
  ("server_host", "server.host"),
  ("server_port", "server.port"),
  ("subprocess_buffer_limit_mb", "server.subprocess_buffer_limit_mb"),
  ("workspace_dirs", "paths.workspace_dirs"),
  ("project_dirs", "paths.workspace_dirs"),
  ("worktree_dir", "paths.worktree_dir"),
  ("backend_options", "backends.options"),
  ("model_preference", "backends.pool"),
  ("home_services", "ui.home_services"),
  ("telegram_chat_id", "telegram.chat_id"),
  ("credentials: telegram_bot_token", "telegram.bot_token"),
  ("credentials: charliebot_access_key", "charliebot.access_key"),
  ("credentials: moonshot_api_key", "moonshot.api_key"),
  ("credentials: aigw_api_key", "aigw.api_key"),
  ("credentials: linear_api_key", "linear.api_key"),
  ("credentials: feishu_app_id", "feishu.app_id"),
  ("credentials: feishu_app_secret", "feishu.app_secret"),
  ("credentials: feishu_refresh_token", "feishu.refresh_token"),
  ("credentials: feishu_user_access_token", "feishu.user_access_token"),
  ("credentials: google_client_id", "google.client_id"),
  ("credentials: google_client_secret", "google.client_secret"),
  ("credentials: google_refresh_token", "google.refresh_token"),
  ("credentials: google_docs_client_id", "google.client_id"),
  ("credentials: google_docs_client_secret", "google.client_secret"),
  ("credentials: google_docs_refresh_token", "google.refresh_token"),
  ("credentials: google_docs_default_folder_id", "google.docs_default_folder_id"),
  ("credentials: twitter_api_key", "twitter.api_key"),
  ("credentials: twitter_api_secret", "twitter.api_secret"),
  ("credentials: twitter_access_token", "twitter.access_token"),
  ("credentials: twitter_access_token_secret", "twitter.access_token_secret"),
  ("publish_dir", "publish.dir"),
  ("public_base_url", "publish.public_base_url"),
  ("code_server_bin", "code_server.bin"),
  ("code_server_config", "code_server.config"),
  ("slack_allowed_user_ids", "slack.allowed_user_ids"),
  ("credentials: slack_bot_token", "slack.bot_token"),
  ("credentials: slack_app_token", "slack.app_token"),
  ("credentials: slack_user_token", "slack.user_token"),
  ("voice_engine", "voice.engine"),
  ("voice_model_id", "voice.model_id"),
  ("claude_accounts", "accounts.claude"),
  ("claude_compaction", "accounts.claude_compaction"),
  ("credentials: gemini_api_key", "gemini.api_key"),
  ("credentials: gemini_model", "gemini.model"),
];

/// Where the retired key `key` moved, as the Python loader prints it, or `None` for a live key.
///
/// A structure key maps to its dotted location; a secret maps to `credentials.yaml <key path>`.
pub fn legacy_destination(key: &str) -> Option<String> {
  let lookup = |name: &str| LEGACY_KEYS.iter().find(|(legacy, _)| *legacy == name).map(|(_, new)| *new);
  if let Some(new) = lookup(key) {
    return Some(new.to_string());
  }
  lookup(&format!("{CREDENTIALS_PREFIX}{key}")).map(|new| format!("credentials.yaml {new}"))
}

/// `src/<kind>/<name>` for the package `src.<kind>.<name>`, under the checkout at `repo_root`.
pub fn package_directory(repo_root: &Path, package: &str) -> PathBuf {
  package.split('.').fold(repo_root.to_path_buf(), |path, part| path.join(part))
}
