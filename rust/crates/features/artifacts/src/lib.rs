//! `src/features/artifacts`: registers the `publish:` config section. The rest of the package is not ported yet.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_registry::{Registry, RegistryError, sidebar::SidebarContribution};
use serde_json::Value;

/// `src/features/artifacts`: the `publish:` section.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection {
    key: "publish",
    legacy_keys: &[("publish_dir", "publish.dir"), ("public_base_url", "publish.public_base_url")],
    parse: raw_section,
  })?;
  registry.sidebar_contribution("artifacts", Arc::new(ArtifactsSidebar))
}

struct ArtifactsSidebar;

impl SidebarContribution for ArtifactsSidebar {
  fn watched_files(&self) -> &'static [&'static str] {
    &["plans.json"]
  }

  fn row_flags(&self, session_dir: &Path, session_id: &str) -> Vec<(&'static str, bool)> {
    vec![("has_pending_plan_approval", pending_plan_approval(&session_dir.join("plans.json"), session_id))]
  }
}

fn pending_plan_approval(path: &Path, session_id: &str) -> bool {
  let raw = match fs::read_to_string(path) {
    Ok(raw) => raw,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
    Err(error) => {
      tracing::warn!(session_id, error = %error, "plan_registry_read_failed");
      return false;
    }
  };
  let registry: Value = match serde_json::from_str(&raw) {
    Ok(registry) => registry,
    Err(error) => {
      tracing::warn!(session_id, error = %error, "plan_registry_read_failed");
      return false;
    }
  };
  let Some(plans) = registry.get("plans").and_then(Value::as_array) else {
    tracing::warn!(session_id, error = "registry plans is not a list", "plan_registry_read_failed");
    return false;
  };
  plans.iter().any(|plan| {
    let Some(versions) = plan.get("versions").and_then(Value::as_array) else { return false };
    if versions.is_empty() {
      return false;
    }
    match plan.get("closed") {
      Some(Value::Object(_)) => false,
      Some(Value::Null) | None => plan.get("takeoff").is_none_or(Value::is_null),
      Some(_) => false,
    }
  })
}

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
