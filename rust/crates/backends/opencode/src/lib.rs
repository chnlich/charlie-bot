//! The opencode backend package (`src/backends/opencode/__init__.py`): its shared server, session backend, usage source
//! and snapshot reading limits.

pub(crate) mod backend;
mod opencode_limits;
mod options;
pub mod relay;
pub(crate) mod server;
mod sqlite;
pub mod usage_logs;
pub mod usage_sweep;

#[cfg(test)]
mod test_store;

#[cfg(test)]
mod test_backend;

use std::path::PathBuf;
use std::sync::Arc;

use charliebot_core::home;
use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::{SNAPSHOT_READING_KIND, UsageSource};
use charliebot_registry::{Registry, RegistryError, registration_step};
use usage_logs::OpencodeLogs;

use self::backend::{OpenCodeFactory, OpenCodeLifecycle};
pub use backend::{OpenCodeBackend, OpenCodeTranslator, reject_permission};
pub use options::OpenCodeOption;
pub use server::{
  OpenCodeHost, OpenCodeProgram, OpenCodeServer, OpenCodeServerPool, ServerTimeouts, Turn, headless_config,
};
pub use usage_logs::USAGE_SOURCE;

/// Register the backend type, shared server factory, usage source and snapshot reading limits.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  let host = registration_step("src.backends.opencode", OpenCodeHost::from_process())?;
  register_with_host(registry, host, ServerTimeouts::default())
}

fn register_with_host(
  registry: &mut Registry,
  host: OpenCodeHost,
  timeouts: ServerTimeouts,
) -> Result<(), RegistryError> {
  register_backend_type(registry, host, timeouts)?;
  register_source(
    registry,
    home::opencode_db()
      .map_err(|error| RegistryError { package: "src.backends.opencode", message: error.to_string() })?,
    PathBuf::from("/proc"),
  )?;
  registry.reading_limits(SNAPSHOT_READING_KIND, opencode_limits::snapshot_reading_limits)
}

fn register_backend_type(
  registry: &mut Registry,
  host: OpenCodeHost,
  timeouts: ServerTimeouts,
) -> Result<(), RegistryError> {
  let pool = OpenCodeServerPool::new(host.clone(), timeouts);
  registry.backend_type(BackendTypeRegistration {
    type_name: "opencode",
    traits: BackendTraits {
      resume: ResumeStyle::NativeId,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: options::option_model,
    factory: Arc::new(OpenCodeFactory::new(host, pool)),
    lifecycle: Some(Arc::new(OpenCodeLifecycle) as Arc<dyn BackendLifecycle>),
    translate_fallback: false,
  })?;
  registry.option_schema("opencode", options::SCHEMA)
}

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
pub(crate) mod tests {
  use charliebot_registry::usage::UsageOps;

  use super::*;

  pub(super) fn register_synthetic(registry: &mut Registry) -> Result<(), RegistryError> {
    let host = OpenCodeHost::with_program(
      PathBuf::from("/synthetic/opencode/bin/opencode"),
      Vec::new(),
      PathBuf::from("/synthetic/opencode/bin/opencode"),
      PathBuf::from("/synthetic/repo"),
      PathBuf::from("/synthetic/home"),
      charliebot_registry::backend::Env::new(),
    );
    register_backend_type(registry, host, ServerTimeouts::default())?;
    register_source(
      registry,
      PathBuf::from("/synthetic/home/opencode/data/opencode/opencode.db"),
      PathBuf::from("/synthetic/proc"),
    )?;
    registry.reading_limits(SNAPSHOT_READING_KIND, opencode_limits::snapshot_reading_limits)
  }

  #[test]
  fn config_loads_extra_body_on_an_endpoint_and_an_opencode_option() {
    use std::fs;

    use charliebot_registry::backend::{BackendFactory, Env, LaunchArgs};
    use serde_json::{Value as JsonValue, json};

    use charliebot_anthropic_compatible as anthropic_compatible;
    use charliebot_claude_code::GATEWAY_EXTRA_BODY_ENV;

    use super::options::model_overrides_config;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(
      home.join("config.yaml"),
      concat!(
        "backends:\n",
        "  options:\n",
        "    - id: endpoint\n",
        "      label: Endpoint\n",
        "      type: cc-anthropic-compatible\n",
        "      model: synthetic/model-1\n",
        "      api_base: http://localhost:8123/api/anthropic-proxy\n",
        "      context_window: 409600\n",
        "      extra_body:\n",
        "        cache:\n",
        "          no-store: true\n",
        "          no-cache: true\n",
        "    - id: oc\n",
        "      label: Opencode\n",
        "      type: opencode\n",
        "      model: fakeprov/fake-model\n",
        "      extra_body:\n",
        "        cache:\n",
        "          no-store: true\n",
        "          no-cache: true\n",
      ),
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.opencode", register_synthetic).unwrap();
    registry.register_package("src.backends.anthropic_compatible", anthropic_compatible::register).unwrap();
    let cfg = registry.load_config(&home).unwrap();
    let extra_body = json!({"cache": {"no-store": true, "no-cache": true}});

    let endpoint = cfg.backends.options.iter().find(|option| option.backend_type == "cc-anthropic-compatible").unwrap();
    let backend = anthropic_compatible::factory::AnthropicCompatibleFactory
      .build(Some(endpoint), &cfg, LaunchArgs::default())
      .unwrap();
    let env = backend.prepare_env(Env::new()).unwrap();
    assert_eq!(serde_json::from_str::<JsonValue>(&env[GATEWAY_EXTRA_BODY_ENV]).unwrap(), extra_body);

    let opencode = cfg.backends.options.iter().find(|option| option.backend_type == "opencode").unwrap();
    let config = model_overrides_config(std::slice::from_ref(opencode)).unwrap();
    assert_eq!(config["providers"]["fakeprov"]["models"]["fake-model"]["body"], extra_body);
  }

  #[test]
  fn registration_matches_the_usage_source() {
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
