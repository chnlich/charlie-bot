//! Test support for this backend and the Anthropic-compatible entry: a synthetic host, a config loaded through the
//! registry, and the fake `claude` that records its argv and environment.
//!
//! The module is under the `test-support` feature, which the dev-dependencies turn on, so a production build compiles
//! none of it.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use charliebot_compat::http_client::{self, Settings};
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::backend::Env;
use charliebot_registry::{Registry, RegistryError};

use crate::usage_quota::QuotaHttp;
use crate::{ClaudeHost, PACKAGE, register_with_host, usage_quota};

pub fn synthetic_host() -> ClaudeHost {
  ClaudeHost { home: PathBuf::from("/synthetic/home"), claude_config_dir: PathBuf::from("/synthetic/home/.claude") }
}

/// An HTTP client that reaches no host: the unit tests never send a request through it.
pub fn quota_http() -> QuotaHttp {
  let client = http_client::client(&Settings::default()).expect("the default client builds");
  QuotaHttp::with_endpoints(client, usage_quota::USAGE_URL, usage_quota::TOKEN_REFRESH_URL)
}

pub fn register_synthetic(registry: &mut Registry) -> Result<(), RegistryError> {
  register_with_host(registry, synthetic_host(), quota_http())
}

/// Loads `home/config.yaml` through the registry, with the `accounts.claude` pool and the worktree directory.
pub fn load_config(home: &Path, accounts: &[(&str, &Path)], worktree_dir: &Path) -> CharlieBotConfig {
  let rows = if accounts.is_empty() {
    "  claude: []\n".to_string()
  } else {
    let mut rows = String::from("  claude:\n");
    for (label, dir) in accounts {
      rows.push_str(&format!("    - label: {label}\n      config_dir: '{}'\n", dir.display()));
    }
    rows
  };
  let text = format!("paths:\n  worktree_dir: '{}'\naccounts:\n{rows}", worktree_dir.display());
  fs::create_dir_all(home).expect("the synthetic home exists");
  fs::write(home.join(charliebot_core::config::CONFIG_FILENAME), text).expect("the synthetic config is written");
  let mut registry = Registry::new();
  registry.register_package(PACKAGE, register_synthetic).expect("the Claude package registers");
  registry.load_config(home).expect("the synthetic config loads")
}

/// The fake-program helper that cargo builds beside the test binary. `CARGO_BIN_EXE_*` names only the test package's
/// own binaries, and the helper belongs to `charliebot-agents`.
pub fn fake_program_bin() -> PathBuf {
  let exe = std::env::current_exe().expect("the test binary path");
  exe
    .ancestors()
    .skip(1)
    .map(|directory| directory.join("fake_program"))
    .find(|candidate| candidate.is_file())
    .expect("cargo builds the fake-program helper into the target directory that holds the test binary")
}

/// The inherited process environment a one-shot receives: the fake `claude` directory on `PATH`.
pub fn inherited_env(bin: &Path) -> Env {
  Env::from([
    ("PATH".to_string(), format!("{}:/usr/bin:/bin", bin.display())),
    ("HOME".to_string(), bin.parent().unwrap().display().to_string()),
  ])
}

/// Stages a fake `claude` that records its argv and environment, reads stdin, prints `ok`, and exits 0. Returns the
/// bin directory and the recorded argv and environment files.
pub fn stage_fake_claude(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
  let bin = root.join("bin");
  fs::create_dir_all(&bin).unwrap();
  std::os::unix::fs::symlink(fake_program_bin(), bin.join("claude")).unwrap();
  let argv = root.join("argv.txt");
  let env_file = root.join("env.txt");
  let stdin = root.join("stdin.txt");
  fs::write(
    bin.join("probe.json"),
    serde_json::json!({"claude": [
      {"record_argv": argv},
      {"record_env": env_file},
      {"record_stdin": stdin},
      {"stdout": "ok"},
      {"exit": 0},
    ]})
    .to_string(),
  )
  .unwrap();
  (bin, argv, env_file)
}

pub fn recorded_lines(path: &Path) -> Vec<String> {
  fs::read_to_string(path).unwrap().lines().map(str::to_string).collect()
}

pub fn recorded_env(path: &Path) -> HashMap<String, String> {
  recorded_lines(path)
    .into_iter()
    .filter_map(|line| line.split_once('=').map(|(name, value)| (name.to_string(), value.to_string())))
    .collect()
}
