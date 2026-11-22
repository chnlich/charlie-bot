use charliebot_core::config::CharlieBotConfig;
use charliebot_core::ui_text::check_label;
use charliebot_registry::{Registry, RegistryError};

pub(super) fn register_claude_code(registry: &mut Registry) -> Result<(), RegistryError> {
  charliebot_claude_code::register(registry)?;
  registry.config_check("check_ui_labels", check_ui_labels)
}

/// The labels of the backend options and pools, then the labels each feature owns: the browser logins and the
/// Claude accounts. Each feature's rule lives beside its section.
fn check_ui_labels(cfg: &CharlieBotConfig) -> Result<(), String> {
  for option in &cfg.backends.options {
    check_label(&format!("backends.options[{}].label", option.id), &option.label)?;
  }
  for pool in &cfg.backends.pools {
    check_label(&format!("backends.pools[{}].label", pool.id), &pool.label)?;
  }
  charliebot_host_auth::check_labels(cfg)?;
  charliebot_claude_code::account_labels::check_labels(cfg)
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::Registry;

  use super::*;

  const LABELS_CONFIG: &str = r#"backends:
  options:
    - id: fable
      label: "Fable"
      type: cc-claude
      model: model
  pools:
    - id: auto
      label: "Automatic"
      rule: rotate
      members: [fable]
host_auth:
  browser_logins:
    - name: browser-login
      label: "Browser login"
      login_command: ["true"]
      approval_url_pattern: "https://.*"
      readback_command: ["true"]
      input_prompt: "Code"
accounts:
  claude:
    - label: "primary"
      config_dir: /synthetic/primary
voice:
  vocabulary: ["飞书"]
"#;

  fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.register_package("features.host_auth", charliebot_host_auth::register).unwrap();
    registry.register_package("features.voice", charliebot_voice::register).unwrap();
    registry.register_package(charliebot_claude_code::PACKAGE, register_claude_code).unwrap();
    registry
  }

  #[test]
  fn rejects_cjk_in_ui_labels_and_loads_voice_vocabulary() {
    let registry = registry();
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join("config.yaml");
    std::fs::write(&config_path, LABELS_CONFIG).unwrap();
    registry.load_config(home.path()).unwrap();

    let cases = [
      ("\"Fable\"", "\"飞书\"", "backends.options[fable].label"),
      ("\"Automatic\"", "\"自动\"", "backends.pools[auto].label"),
      ("\"Browser login\"", "\"ログイン\"", "host_auth.browser_logins[browser-login].label"),
      ("\"Code\"", "\"코드\"", "host_auth.browser_logins[browser-login].input_prompt"),
      ("\"primary\"", "\"主账号\"", "accounts.claude[主账号].label"),
    ];
    for (source, replacement, path) in cases {
      let config = LABELS_CONFIG.replace(source, replacement);
      std::fs::write(&config_path, config).unwrap();
      let error = registry.load_config(home.path()).unwrap_err().to_string();
      assert!(error.contains(path), "expected {path} in {error}");
    }
  }

  #[test]
  fn failed_label_reload_keeps_the_previous_config() {
    let registry = Arc::new(registry());
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join("config.yaml");
    std::fs::write(&config_path, LABELS_CONFIG).unwrap();
    let registry_for_load = Arc::clone(&registry);
    let home_path = home.path().to_path_buf();
    let config = HotReload::new(
      config_path.clone(),
      "test_config_reload_failed",
      Box::new(move || registry_for_load.load_config(&home_path)),
    );

    let previous = config.get().unwrap();
    std::fs::write(&config_path, LABELS_CONFIG.replace("\"Automatic\"", "\"自动\"")).unwrap();
    let current = config.get().unwrap();
    assert!(Arc::ptr_eq(&previous, &current));
    assert_eq!(current.backends.pools[0].label, "Automatic");
    let error = registry.load_config(home.path()).unwrap_err().to_string();
    assert!(error.contains("backends.pools[auto].label"), "{error}");
  }
}
