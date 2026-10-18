//! The codex backend package (`src/backends/codex/__init__.py`): the backend type, its usage resolver, and its usage
//! source with the Codex quota account.

mod backend;
pub mod factory;
mod options;
pub mod pricing;
pub mod resolver;
mod rollout;
mod text;
mod translate;
pub mod usage_logs;
pub mod usage_quota;
mod usage_sweep;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::UsageSource;
use charliebot_registry::{Registry, RegistryError};

pub use backend::CodexBackend;
pub use resolver::{CodexSessionHost, CodexUsageResolver, UsageHost};
pub use translate::FILE_WRITE;
pub use usage_logs::session_id;

use factory::CodexFactory;

/// The usage source's name: the ledger's source value and the usage page's card title.
pub const USAGE_SOURCE: &str = "Codex";

/// The process environment the codex package reads: PATH to find the codex binary, and HOME for `~/.local/bin` and
/// `~/.codex`.
#[derive(Clone, Debug)]
pub struct CodexHost {
  pub path: String,
  pub home: PathBuf,
}

impl CodexHost {
  /// The host of the running server. PATH defaults to Python's `os.defpath` when unset; HOME must be set.
  pub fn from_process() -> anyhow::Result<Self> {
    let home = std::env::var("HOME").context("HOME must be set for the codex package")?;
    let path = match std::env::var("PATH") {
      Ok(path) => path,
      Err(std::env::VarError::NotPresent) => "/bin:/usr/bin".to_string(),
      Err(error) => return Err(error).context("PATH must be valid Unicode for the codex package"),
    };
    Ok(Self { path, home: PathBuf::from(home) })
  }

  /// `Path.home() / ".local" / "bin"`: the directory the child PATH gains, and the binary's fallback.
  pub fn user_local_bin(&self) -> PathBuf {
    self.home.join(".local").join("bin")
  }

  /// `Path.home() / ".codex"`: the default codex home that holds the rollouts.
  pub fn codex_home(&self) -> PathBuf {
    self.home.join(".codex")
  }
}

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
  registry.usage_resolver("codex", Arc::new(CodexUsageResolver::new(host.codex_home())))?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["codex-"],
    run_logs_only: false,
    implementation: Some(Arc::new(usage_logs::CodexLogs::new(host.codex_home()))),
  })?;
  registry.attribute_backend_type("codex", USAGE_SOURCE)?;
  registry.pre_recovery_step("codex_usage_host", resolver::bind_usage_host)
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_registry::backend::{BackendTraits, ResumeStyle};
  use charliebot_registry::usage::UsageOps;

  #[test]
  fn host_paths_follow_the_home_directory() {
    let host = CodexHost { path: "/bin".to_string(), home: PathBuf::from("/synthetic/home") };
    assert_eq!(host.user_local_bin(), PathBuf::from("/synthetic/home/.local/bin"));
    assert_eq!(host.codex_home(), PathBuf::from("/synthetic/home/.codex"));
  }

  fn register_synthetic_host(registry: &mut Registry) -> Result<(), RegistryError> {
    register_with_host(
      registry,
      &CodexHost { path: "/synthetic/bin".to_string(), home: PathBuf::from("/synthetic/home") },
    )
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
}
