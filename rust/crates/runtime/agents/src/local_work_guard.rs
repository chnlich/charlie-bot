//! The local work guard: the environment that makes an agent's shell refuse a compile or a test run on this host.
//!
//! `configure` reads the switch when the server loads its configuration. `child_env` is the registry edit that adds
//! the variables to each agent child process. The server's own environment never receives them, so the web terminal
//! and the one-shot calls run without them. The variables point into `dev/local-work-guard/` of the runtime checkout,
//! the copy the server reads prompts and skills from. Each refusal script names the toolchains it refuses by the list
//! in `dev/remote-run/README.md`. The build host is not part of this environment: the refusal and the runner read
//! `remote_run.host` from `config.yaml` on each run.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use charliebot_core::config::ServerConfig;
use charliebot_registry::backend::Env;

/// The refusal scripts' directory, relative to the runtime checkout.
const GUARD_DIR: &str = "dev/local-work-guard";

/// The guard the server configured: `None` when `server.enable_local_work_guard` is false.
static GUARD: OnceLock<Option<LocalWorkGuard>> = OnceLock::new();

/// The guard's settings: the refusal directory that its variables point into.
#[derive(Debug, PartialEq, Eq)]
struct LocalWorkGuard {
  dir: PathBuf,
}

/// The guard switch of `server` read for the checkout at `repo_root`. The server calls this once, before any agent
/// run; an error stops the start.
pub fn configure(repo_root: &Path, server: &ServerConfig) -> Result<(), String> {
  let guard = LocalWorkGuard::from_server(repo_root, server)?;
  GUARD.set(guard).map_err(|_| "the local work guard is configured twice in one process".to_string())
}

/// The registry's child-environment edit: adds the guard's variables to an agent child's environment when the guard is
/// on. The server configures the guard before it runs any agent.
pub fn child_env(env: &mut Env) {
  let guard = GUARD.get().expect("the server configures the local work guard before any agent run");
  if let Some(guard) = guard {
    guard.apply(env);
  }
}

impl LocalWorkGuard {
  /// The guard for `server`, or `None` when its switch is off.
  fn from_server(repo_root: &Path, server: &ServerConfig) -> Result<Option<Self>, String> {
    if !server.enable_local_work_guard {
      return Ok(None);
    }
    let dir = repo_root.join(GUARD_DIR);
    // NODE_OPTIONS and MAKEFILES split their values on spaces, so a space in the path would drop the refusal silently.
    if dir.to_string_lossy().contains(' ') {
      return Err(format!(
        "the runtime checkout path has a space, which the local work guard cannot carry: {}",
        dir.display()
      ));
    }
    Ok(Some(Self { dir }))
  }

  /// Adds the refusal variables to `env`. An entry already set keeps its value after the guard's entry.
  fn apply(&self, env: &mut Env) {
    let dir = self.dir.display();
    env.insert("RUSTUP_TOOLCHAIN".to_string(), format!("{dir}/rustup"));
    env.insert("RUSTC_WRAPPER".to_string(), format!("{dir}/rustup/rustc-wrapper"));
    prepend(env, "PYTEST_PLUGINS", ",", "refuse_local_tests");
    prepend(env, "PYTHONPATH", ":", &format!("{dir}/python"));
    prepend(env, "NODE_OPTIONS", " ", &format!("--require={dir}/node/refuse-local-tests.cjs"));
    prepend(env, "MAKEFILES", " ", &format!("{dir}/make/refuse.mk"));
    prepend(env, "GOFLAGS", " ", &format!("-toolexec={dir}/go/toolexec.sh"));
    env.insert("CC".to_string(), format!("{dir}/bin/cc"));
    env.insert("CXX".to_string(), format!("{dir}/bin/c++"));
    prepend(env, "PATH", ":", &format!("{dir}/bin"));
  }
}

/// The new value of `name`: `entry` before the variable's current value, joined with `separator`.
fn prepend(env: &mut Env, name: &str, separator: &str, entry: &str) {
  let value = match env.get(name) {
    Some(current) if !current.is_empty() => format!("{entry}{separator}{current}"),
    _ => entry.to_string(),
  };
  env.insert(name.to_string(), value);
}

#[cfg(test)]
mod tests {
  use super::*;

  fn server(yaml: &str) -> ServerConfig {
    serde_yaml::from_str(yaml).unwrap()
  }

  #[test]
  fn the_guard_sets_each_refusal_variable_and_keeps_the_values_already_set() {
    let guard = LocalWorkGuard { dir: PathBuf::from("/checkout/dev/local-work-guard") };
    let mut env = Env::from([
      ("PATH".to_string(), "/usr/bin".to_string()),
      ("PYTHONPATH".to_string(), "/opt/lib".to_string()),
      ("NODE_OPTIONS".to_string(), "--max-old-space-size=4096".to_string()),
    ]);

    guard.apply(&mut env);

    let guard_dir = "/checkout/dev/local-work-guard";
    let expected = Env::from([
      ("CC".to_string(), format!("{guard_dir}/bin/cc")),
      ("CXX".to_string(), format!("{guard_dir}/bin/c++")),
      ("GOFLAGS".to_string(), format!("-toolexec={guard_dir}/go/toolexec.sh")),
      ("MAKEFILES".to_string(), format!("{guard_dir}/make/refuse.mk")),
      (
        "NODE_OPTIONS".to_string(),
        format!("--require={guard_dir}/node/refuse-local-tests.cjs --max-old-space-size=4096"),
      ),
      ("PATH".to_string(), format!("{guard_dir}/bin:/usr/bin")),
      ("PYTEST_PLUGINS".to_string(), "refuse_local_tests".to_string()),
      ("PYTHONPATH".to_string(), format!("{guard_dir}/python:/opt/lib")),
      ("RUSTC_WRAPPER".to_string(), format!("{guard_dir}/rustup/rustc-wrapper")),
      ("RUSTUP_TOOLCHAIN".to_string(), format!("{guard_dir}/rustup")),
    ]);
    assert_eq!(env, expected);
  }

  #[test]
  fn the_switch_off_configures_no_guard_and_the_switch_on_needs_no_runner_key() {
    let root = Path::new("/checkout");
    assert_eq!(LocalWorkGuard::from_server(root, &server("enable_local_work_guard: false")).unwrap(), None);
    assert_eq!(
      LocalWorkGuard::from_server(root, &server("enable_local_work_guard: true")).unwrap(),
      Some(LocalWorkGuard { dir: PathBuf::from("/checkout/dev/local-work-guard") })
    );
  }
}
