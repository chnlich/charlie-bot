//! The opencode package's registrations: the backend type with its shared server, the usage source with its member
//! reading, and the snapshot reading limits.

use std::path::PathBuf;
use std::sync::Arc;

use charliebot_core::home;
use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::{NativeCacheRead, SNAPSHOT_READING_KIND, UsageSource};
use charliebot_registry::{Registry, RegistryError, registration_step};

use crate::backend::{OpenCodeFactory, OpenCodeLifecycle};
use crate::server::{OpenCodeHost, OpenCodeServerPool, ServerTimeouts};
use crate::usage_logs::{OpencodeLogs, USAGE_SOURCE};
use crate::{opencode_limits, options, usage_quota};

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.opencode";

/// The registration of the backend type, shared server factory, usage source and snapshot reading limits.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  let host = registration_step(PACKAGE, OpenCodeHost::from_process())?;
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
    home::opencode_db().map_err(|error| RegistryError { package: PACKAGE, message: error.to_string() })?,
    PathBuf::from("/proc"),
  )?;
  registry.reading_limits(SNAPSHOT_READING_KIND, opencode_limits::snapshot_reading_limits)
}

pub(crate) fn register_backend_type(
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

/// The registration of the opencode usage source, which attributes the `opencode` backend type to it. The database
/// is the profile's `opencode_db()`, resolved once here.
pub fn register_usage_source(registry: &mut Registry) -> Result<(), RegistryError> {
  let db = home::opencode_db().map_err(|error| RegistryError { package: PACKAGE, message: error.to_string() })?;
  register_source(registry, db, PathBuf::from("/proc"))
}

/// The usage-source registration that [`register_usage_source`] performs, over an explicit database and process root
/// for the writer count.
pub(crate) fn register_source(registry: &mut Registry, db: PathBuf, proc_root: PathBuf) -> Result<(), RegistryError> {
  let quota_state = Arc::new(usage_quota::OpenCodeQuotaState::default());
  registry.member_reading("opencode", usage_quota::OpenCodeQuotaState::member_reading(Arc::clone(&quota_state)))?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["opencode-", "oc-"],
    run_logs_only: false,
    native_input: charliebot_registry::usage::NativeInput::Inclusive,
    native_scope: charliebot_registry::usage::NativeScope::PerRun,
    native_cache_read: NativeCacheRead::CachedTokens,
    implementation: Some(Arc::new(OpencodeLogs::new(db, proc_root, quota_state))),
  })?;
  registry.attribute_backend_type("opencode", USAGE_SOURCE)
}
