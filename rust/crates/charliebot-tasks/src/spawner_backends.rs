//! Backend-resolution refusals used by the manager spawner.

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_store::metadata::SessionMetadata;

/// Return the refusal core when a session pins a backend id absent from `config.yaml`.
pub fn unknown_backend_pin_refusal(backend_id: &str, fallback_id: &str) -> String {
  format!("'{backend_id}' is not in config.yaml backends.options — refusing to substitute '{fallback_id}'")
}

/// Resolve the exact backend and recorded model on one run.
pub fn resolve_backend_option(
  cfg: &CharlieBotConfig,
  backend_id: &str,
  model: Option<&str>,
) -> anyhow::Result<BackendOption> {
  if backend_id.is_empty() {
    anyhow::bail!("resolved backend is required");
  }
  let mut option = cfg
    .get_backend_option(backend_id)
    .cloned()
    .ok_or_else(|| anyhow::anyhow!("resolved backend '{backend_id}' is not in backends.options"))?;
  let model = model.filter(|model| !model.is_empty()).ok_or_else(|| anyhow::anyhow!("resolved model is required"))?;
  option.model = model.to_string();
  Ok(option)
}

/// Resolve a session's pinned backend or its configured first backend.
pub fn resolve_session_default_backend_model(
  cfg: &CharlieBotConfig,
  session_meta: &SessionMetadata,
) -> anyhow::Result<(String, Option<String>)> {
  let option = if !session_meta.backend.is_empty() {
    let backend_id = session_meta.backend.as_str();
    match cfg.get_backend_option(backend_id) {
      Some(option) => option,
      None if cfg.backends.options.is_empty() => {
        anyhow::bail!("session backend resolution requires a configured backends.options entry");
      }
      None => {
        let fallback = &cfg.backends.options[0].id;
        tracing::error!(
          target: "charliebot_tasks",
          session_id = %session_meta.id,
          stored = backend_id,
          refused_substitute = fallback,
          "session_backend_unresolved"
        );
        anyhow::bail!("session backend {}", unknown_backend_pin_refusal(backend_id, fallback));
      }
    }
  } else {
    cfg
      .backends
      .options
      .first()
      .ok_or_else(|| anyhow::anyhow!("session backend resolution requires a configured backends.options entry"))?
  };
  let model = if option.model.is_empty() {
    anyhow::bail!("session backend '{}' has no default model", option.id);
  } else {
    option.model.clone()
  };
  Ok((option.id.clone(), Some(model)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn refusal_names_the_missing_pin_and_rejected_substitute() {
    assert_eq!(
      unknown_backend_pin_refusal("synthetic-missing", "synthetic-default"),
      "'synthetic-missing' is not in config.yaml backends.options — refusing to substitute 'synthetic-default'"
    );
  }

  #[test]
  fn resolved_backend_requires_an_explicit_backend_and_model() {
    let cfg = test_config();
    assert_eq!(resolve_backend_option(&cfg, "synthetic", Some("model-2")).unwrap().model, "model-2");
    assert_eq!(
      resolve_backend_option(&cfg, "", Some("model-2")).unwrap_err().to_string(),
      "resolved backend is required"
    );
    assert_eq!(resolve_backend_option(&cfg, "synthetic", None).unwrap_err().to_string(), "resolved model is required");
  }

  fn test_config() -> CharlieBotConfig {
    use charliebot_core::config::{BackendsConfig, PathsConfig, ServerConfig, TelegramConfig, UiConfig};
    use std::path::PathBuf;

    CharlieBotConfig {
      charliebot_home: PathBuf::from("/synthetic/home"),
      headless_chrome_bin: "chromium".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18600,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 1,
        session_swap_max_mb: 0,
        session_tasks_max: 1,
        min_free_disk_gib: 0,
      },
      paths: PathsConfig { workspace_dirs: Vec::new(), worktree_dir: "/synthetic/worktrees".to_string() },
      backends: BackendsConfig {
        pool: Vec::new(),
        options: vec![BackendOption {
          id: "synthetic".to_string(),
          label: "synthetic".to_string(),
          model: "model-1".to_string(),
          prompt_overlay: Some("none".to_string()),
          backend_type: "synthetic".to_string(),
          type_fields: Default::default(),
        }],
      },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Default::default(),
    }
  }
}
