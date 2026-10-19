//! The opencode backend package (`src/backends/opencode/__init__.py`), as far as this crate ports it: its usage source.
//! The backend type, its factory and its reading limits stay with the opencode backend port.

pub mod usage_logs;
pub mod usage_sweep;

#[cfg(test)]
mod test_store;

use std::path::PathBuf;
use std::sync::Arc;

use charliebot_core::home;
use charliebot_registry::usage::UsageSource;
use charliebot_registry::{Registry, RegistryError};
use usage_logs::OpencodeLogs;

/// The usage source's name: the ledger's source value and the usage page's card title.
pub const USAGE_SOURCE: &str = "opencode";

/// Registers the opencode usage source and attributes the `opencode` backend type to it (`__init__.py:24-30`). The
/// database is the profile's `opencode_db()`, resolved once here.
pub fn register_usage_source(registry: &mut Registry) -> Result<(), RegistryError> {
  let db = home::opencode_db()
    .map_err(|error| RegistryError { package: "src.backends.opencode", message: error.to_string() })?;
  register_source(registry, db, PathBuf::from("/proc"))
}

/// [`register_usage_source`] over an explicit database and process root for the writer count.
fn register_source(registry: &mut Registry, db: PathBuf, proc_root: PathBuf) -> Result<(), RegistryError> {
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["opencode-", "oc-"],
    run_logs_only: false,
    implementation: Some(Arc::new(OpencodeLogs::new(db, proc_root))),
  })?;
  registry.attribute_backend_type("opencode", USAGE_SOURCE)
}

#[cfg(test)]
mod tests {
  use charliebot_registry::usage::UsageOps;

  use super::*;

  fn register_synthetic(registry: &mut Registry) -> Result<(), RegistryError> {
    register_source(
      registry,
      PathBuf::from("/synthetic/home/opencode/data/opencode/opencode.db"),
      PathBuf::from("/synthetic/proc"),
    )
  }

  #[test]
  fn registration_matches_the_python_usage_source() {
    // tests/test_usage_sources.py: the name, id prefixes and run_logs_only of the opencode source, and the attribution
    // of the opencode backend type (`src/backends/opencode/__init__.py:24-30`).
    let mut registry = Registry::new();
    registry.register_package("src.backends.opencode", register_synthetic).unwrap();

    let source = registry.usage_source_for("opencode").unwrap().unwrap();
    assert_eq!(source.name, USAGE_SOURCE);
    assert_eq!(source.id_prefixes, &["opencode-", "oc-"]);
    assert!(!source.run_logs_only);
    assert_eq!(
      source.implementation.as_ref().unwrap().ops(),
      UsageOps { logs: true, read: true, sweep: true, ..UsageOps::default() }
    );
  }
}
