//! The `devin` backend type: the Devin CLI in print mode, run headless with every tool approved. The CLI prints the
//! final answer on stdout and writes the turn's trajectory to an export file, which the runner reads once the process
//! has exited. Token usage comes from CharlieBot's run logs; subscription quota reads use the Devin status endpoint.

mod devin_cli;
mod export;
mod factory;
mod options;
mod usage_quota;

use std::sync::Arc;

use charliebot_agents::backend_availability::BackendAvailability;
use charliebot_agents::cli_base::CliHost;
use charliebot_core::home;
use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::UsageSource;
use charliebot_registry::{Registry, RegistryError, registration_step};

/// The ledger's source value and the usage page's card title for devin's usage.
pub const USAGE_SOURCE: &str = "Devin";

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.devin";

/// The registration of the `devin` type and the Devin usage source, which attributes the type's usage.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  register_with_host(registry, registration_step(PACKAGE, CliHost::from_process())?)
}

/// [`register`] over an explicit host.
pub fn register_with_host(registry: &mut Registry, host: CliHost) -> Result<(), RegistryError> {
  let profile_home = registration_step(PACKAGE, home::charliebot_home_dir().map_err(anyhow::Error::new))?;
  let availability = registration_step(PACKAGE, BackendAvailability::load(&profile_home))?;
  register_over(registry, host, availability)
}

/// The registration over an explicit host and availability record (`register_with_host` reads both from the profile).
fn register_over(
  registry: &mut Registry,
  host: CliHost,
  availability: BackendAvailability,
) -> Result<(), RegistryError> {
  let state = Arc::new(usage_quota::DevinQuotaState::default());
  let account = Arc::new(usage_quota::DevinQuotaAccount::new(
    Arc::clone(&state),
    host.home.join(".local/share/devin/credentials.toml"),
    host.clone(),
    Arc::new(availability),
  ));
  registry.backend_type(BackendTypeRegistration {
    type_name: "devin",
    traits: BackendTraits {
      resume: ResumeStyle::NativeId,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: options::option_model,
    factory: Arc::new(factory::DevinFactory::new(host)),
    lifecycle: None,
    translate_fallback: false,
  })?;
  registry.option_schema("devin", options::SCHEMA)?;
  registry.member_reading("devin", usage_quota::DevinUsageSource::member_reading(Arc::clone(&state)))?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["devin-"],
    run_logs_only: true,
    implementation: Some(Arc::new(usage_quota::DevinUsageSource::new(account))),
  })?;
  registry.attribute_backend_type("devin", USAGE_SOURCE)
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::{Path, PathBuf};

  use super::*;

  /// The Devin registration over a synthetic host and profile home. Neither path holds a file, so the availability
  /// record is empty.
  fn register_synthetic(registry: &mut Registry) -> Result<(), RegistryError> {
    let host = CliHost { path: String::new(), home: PathBuf::from("/synthetic/home") };
    let availability = BackendAvailability::load(Path::new("/synthetic/charliebot")).unwrap();
    register_over(registry, host, availability)
  }

  #[test]
  fn quota_account_is_listed_only_when_an_option_draws_on_devin() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.register_package(PACKAGE, register_synthetic).unwrap();

    let without = root.path().join("without");
    fs::create_dir_all(&without).unwrap();
    let cfg = registry.load_config(&without).unwrap();
    assert!(registry.quota_accounts(&cfg).unwrap().is_empty());

    let with = root.path().join("with");
    fs::create_dir_all(&with).unwrap();
    fs::write(
      with.join("config.yaml"),
      concat!(
        "backends:\n",
        "  options:\n",
        "    - id: dv\n",
        "      label: Devin\n",
        "      type: devin\n",
        "      model: devin-model\n",
      ),
    )
    .unwrap();
    let cfg = registry.load_config(&with).unwrap();
    let accounts = registry.quota_accounts(&cfg).unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].provider(), "devin");
  }
}
