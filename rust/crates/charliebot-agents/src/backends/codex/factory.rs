//! The codex backend factory (`src/backends/codex/factory.py`) and the codex binary lookup (`base.resolve_binary`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::CodexHost;
use super::backend::CodexBackend;
use super::options;

/// Builds one codex backend per launch, resolving the codex binary from the package's host.
pub struct CodexFactory {
  host: CodexHost,
}

impl CodexFactory {
  pub fn new(host: CodexHost) -> Self {
    Self { host }
  }
}

impl BackendFactory for CodexFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("the codex backend type needs its backends.options entry")?;
    if option.model.is_empty() {
      bail!("backend '{}' has no default model", option.id);
    }
    let fields = options::parse(&option.type_fields).map_err(anyhow::Error::msg)?;
    let user_local_bin = self.host.user_local_bin();
    let codex_bin = resolve_binary("codex", &self.host.path, &user_local_bin)?;
    Ok(Box::new(CodexBackend::from_launch(codex_bin, &user_local_bin, &option.model, &fields, launch)?))
  }
}

/// The first executable `name` on `path`, else `fallback_dir/name` when that exists (`base.resolve_binary`).
pub fn resolve_binary(name: &str, path: &str, fallback_dir: &Path) -> anyhow::Result<PathBuf> {
  for dir in path.split(':') {
    let candidate = Path::new(dir).join(name);
    if is_executable_file(&candidate) {
      return Ok(candidate);
    }
  }
  let fallback = fallback_dir.join(name);
  if fallback.exists() {
    return Ok(fallback);
  }
  bail!("{name} binary not found on PATH or at {}", fallback.display())
}

fn is_executable_file(path: &Path) -> bool {
  fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn binary_resolves_from_path_then_the_fallback_dir() {
    let root = tempfile::tempdir().unwrap();
    let on_path = root.path().join("path-bin");
    let fallback = root.path().join("local-bin");
    fs::create_dir_all(&on_path).unwrap();
    fs::create_dir_all(&fallback).unwrap();
    fs::write(fallback.join("codex"), "").unwrap();
    assert_eq!(resolve_binary("codex", &on_path.to_string_lossy(), &fallback).unwrap(), fallback.join("codex"));
    fs::write(on_path.join("codex"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(on_path.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(resolve_binary("codex", &on_path.to_string_lossy(), &fallback).unwrap(), on_path.join("codex"));
    assert!(resolve_binary("codex", "", &root.path().join("missing")).is_err());
  }
}
