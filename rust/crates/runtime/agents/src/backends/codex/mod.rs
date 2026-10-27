//! The codex backend package (`src/backends/codex/__init__.py`): the backend type, its usage resolver, and its usage
//! source with the Codex quota account.

mod backend;
pub mod factory;
mod host;
mod options;
pub mod pricing;
pub mod resolver;
mod rollout;
mod text;
mod translate;
pub mod usage_logs;
pub mod usage_quota;
mod usage_sweep;

use std::sync::Arc;

use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::UsageSource;
use charliebot_registry::{Registry, RegistryError};

pub use backend::CodexBackend;
pub use host::CodexHost;
pub use resolver::{CodexSessionHost, CodexUsageResolver};
pub use translate::FILE_WRITE;
pub use usage_logs::session_id;

use factory::CodexFactory;

/// The usage source's name: the ledger's source value and the usage page's card title.
pub const USAGE_SOURCE: &str = "Codex";

/// Registers the codex backend type, its usage resolver, its usage source, and the source's attribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  let host = CodexHost::from_process()
    .map_err(|error| RegistryError { package: "src.backends.codex", message: format!("{error:#}") })?;
  register_with_host(registry, &host)
}

/// [`register`] over an explicit host.
pub fn register_with_host(registry: &mut Registry, host: &CodexHost) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "codex",
    traits: BackendTraits {
      resume: ResumeStyle::NativeId,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: Some("codex"),
      manual_compact: false,
      compact_hint: Some("codex only compacts automatically — tune model_auto_compact_token_limit"),
    },
    option_model: options::option_model,
    factory: Arc::new(CodexFactory::new(host.clone())),
    lifecycle: None,
    translate_fallback: false,
  })?;
  let codex_home = host.codex_home();
  registry
    .usage_resolver("codex", Arc::new(move |usage_host| resolver::usage_resolver(codex_home.clone(), usage_host)))?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["codex-"],
    run_logs_only: false,
    implementation: Some(Arc::new(usage_logs::CodexLogs::new(host.codex_home()))),
  })?;
  registry.attribute_backend_type("codex", USAGE_SOURCE)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;
  use std::sync::OnceLock;

  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::backend::{BackendTraits, ResumeStyle, UsageResolverHost};
  use charliebot_registry::usage::UsageOps;
  use charliebot_store::Store;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;

  fn synthetic_home() -> &'static std::path::Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| tempfile::tempdir().unwrap()).path()
  }

  #[test]
  fn host_paths_follow_the_home_directory() {
    let host = CodexHost { path: "/bin".to_string(), home: PathBuf::from("/synthetic/home") };
    assert_eq!(host.user_local_bin(), PathBuf::from("/synthetic/home/.local/bin"));
    assert_eq!(host.codex_home(), PathBuf::from("/synthetic/home/.codex"));
  }

  fn register_synthetic_host(registry: &mut Registry) -> Result<(), RegistryError> {
    register_with_host(
      registry,
      &CodexHost { path: "/synthetic/bin".to_string(), home: synthetic_home().to_path_buf() },
    )
  }

  fn usage_resolver_host(registry: Arc<Registry>, home: &std::path::Path) -> UsageResolverHost {
    let config_value = registry.load_config(home).unwrap();
    let config_file = config_value.config_file();
    let reload_value = config_value.clone();
    let config = Arc::new(HotReload::new(
      config_file,
      "codex_usage_resolver_test_config_reload_failed",
      Box::new(move || Ok(reload_value.clone())),
    ));
    let slots = registry.metadata_slots();
    let hooks = charliebot_registry::store_hooks::store_hooks(Arc::clone(&registry));
    let store = Store::new(config_value, slots, hooks).unwrap();
    UsageResolverHost { config, store: Arc::new(store) }
  }

  #[test]
  fn registration_matches_the_python_backend_and_usage_source() {
    let mut registry = Registry::new();
    registry.register_package("src.backends.codex", register_synthetic_host).unwrap();

    let backend = registry.backend_type_of("codex").unwrap();
    assert_eq!(
      backend.traits,
      BackendTraits {
        resume: ResumeStyle::NativeId,
        restart_reattach: true,
        preassigned_session_id: false,
        family_prefix: Some("codex"),
        manual_compact: false,
        compact_hint: Some("codex only compacts automatically — tune model_auto_compact_token_limit"),
      }
    );
    let fields = serde_yaml::from_str("model_auto_compact_token_limit: 150000").unwrap();
    (backend.option_model)(&fields).unwrap();
    let unknown = serde_yaml::from_str("effort: high").unwrap();
    assert!((backend.option_model)(&unknown).is_err());

    assert!(registry.usage_resolver_for("codex").is_some());
    let source = registry.usage_source_for("codex").unwrap().unwrap();
    assert_eq!(source.name, USAGE_SOURCE);
    assert_eq!(source.id_prefixes, &["codex-"]);
    assert!(!source.run_logs_only);
    assert_eq!(
      source.implementation.as_ref().unwrap().ops(),
      UsageOps { logs: true, read: true, live_cli_sessions: true, quota_accounts: true, sweep: true }
    );
  }

  #[test]
  fn registered_usage_resolver_reads_the_synthetic_host_codex_home() {
    let home = synthetic_home();
    let thread_id = uuid::Uuid::new_v4().to_string();
    let rollout =
      home.join(".codex/sessions/2026/10/18").join(format!("rollout-2026-10-18T10-00-00-{thread_id}.jsonl"));
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let records = [
      json!({"type": "turn_context", "payload": {"model": "gpt-5.5"}}),
      json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
        "last_token_usage": {"input_tokens": 321}, "model_context_window": 12345
      }}}),
    ];
    let contents: String = records.iter().map(|record| format!("{record}\n")).collect();
    std::fs::write(rollout, contents).unwrap();

    let mut registry = Registry::new();
    registry.register_package("src.backends.codex", register_synthetic_host).unwrap();
    let registry = Arc::new(registry);
    let host = usage_resolver_host(Arc::clone(&registry), home);
    let constructor = registry.usage_resolver_for("codex").unwrap();
    let resolver = constructor(&host);
    let mut meta = SessionMetadata::new("synthetic-session", "Synthetic", TaskProfile::Manager);
    meta.cc_session_id = Some(thread_id);

    let usage = resolver.resolve("synthetic-session", &meta, &[]).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], json!(321));
    assert_eq!(usage["context_full"], json!(12345));
  }
}
