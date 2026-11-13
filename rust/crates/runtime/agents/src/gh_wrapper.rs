//! The agent `gh` wrapper: text-carrying GitHub writes use the App token when configured, otherwise the host login.
//!
//! The wrapper script is `rust/crates/runtime/agents/gh_wrapper/gh`, in the directory this module owns.
//! `agent_environment` puts its directory first on the PATH of every agent process and records the script path in
//! the environment; the spawn path
//! (`AgentRunner.run`) calls [`require_gh_wrapper`] once after the backend's own environment edits, moves the wrapper
//! directory back to the front and refuses a process whose PATH resolves `gh` anywhere else. The script sends each
//! text-carrying GitHub write to the App through the token command named by [`TOKEN_COMMAND_VAR`] (the host config's
//! `github.app_token_command`) when configured; otherwise every call passes through as the host's personal login.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_registry::backend::Env;

/// The repository directory that holds the wrapper script.
pub const WRAPPER_REPO_DIR: &str = "rust/crates/runtime/agents/gh_wrapper";
/// The environment variable that names the wrapper script for the spawn check. `agent_environment` sets it; an
/// environment without it was not built by the server and carries no wrapper contract.
pub const WRAPPER_PATH_VAR: &str = "CHARLIEBOT_GH_WRAPPER";
/// The environment variable that names the App token command the wrapper runs. The spawn path sets it from the live
/// host config's [`CONFIG_KEY`] (`agent_environment::spawn_base_environment`); when unset, the wrapper uses the host
/// gh login.
pub const TOKEN_COMMAND_VAR: &str = "CHARLIEBOT_GH_APP_TOKEN_CMD";
/// The host config key that names the App token command.
pub const CONFIG_KEY: &str = "github.app_token_command";

/// The wrapper directory under `repo_root`.
pub fn dir_under(repo_root: &Path) -> PathBuf {
  repo_root.join(WRAPPER_REPO_DIR)
}

/// Set or clear the App token command in an agent environment.
///
/// The host config owns the value: `Some(command)` inserts [`TOKEN_COMMAND_VAR`], and `None` or an empty value
/// removes it, so the wrapper uses the host gh login when no App token command is configured.
pub fn apply_token_command(values: &mut Env, command: Option<&str>) {
  match command {
    Some(command) if !command.is_empty() => {
      values.insert(TOKEN_COMMAND_VAR.to_string(), command.to_string());
    }
    _ => {
      values.remove(TOKEN_COMMAND_VAR);
    }
  }
}

/// Refuse a start without an executable wrapper script, the way `require_agent_cli` does for the CLI.
pub fn require_installed(dir: &Path) -> anyhow::Result<()> {
  let script = dir.join("gh");
  let metadata =
    fs::metadata(&script).with_context(|| format!("the agent gh wrapper must exist at {}", script.display()))?;
  if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
    bail!("the agent gh wrapper must be an executable file at {}", script.display());
  }
  Ok(())
}

/// Require a spawn's PATH to resolve `gh` to the wrapper, with the wrapper's directory first in the final PATH.
///
/// A backend's `prepare_env` runs last and can prepend a directory: codex, charlie-code and antigravity put the
/// user's `~/.local/bin` first, where the personal `gh` lives. Moving the wrapper directory back to the front keeps
/// `gh` resolving to the wrapper without changing the other entries. A PATH that does not carry the wrapper
/// directory at all is left alone, so the resolution check below reports the `gh` it does resolve.
///
/// An environment without [`WRAPPER_PATH_VAR`] was not built by an agent environment builder: it carries no wrapper
/// contract, so the spawn fails rather than letting `gh` resolve to the operator's personal login.
pub fn require_gh_wrapper(env: &mut Env) -> anyhow::Result<()> {
  let Some(wrapper) = env.get(WRAPPER_PATH_VAR).cloned() else {
    bail!("{WRAPPER_PATH_VAR} is not set, so the spawn has no agent gh wrapper; refusing to start the agent process");
  };
  let wrapper_path = PathBuf::from(&wrapper);
  let Some(wrapper_dir) = wrapper_path.parent() else {
    bail!("the agent gh wrapper path {wrapper} names no directory");
  };
  let Some(path) = env.get("PATH").cloned() else {
    bail!("PATH is not set, so gh cannot resolve to the agent gh wrapper at {wrapper}");
  };
  let mut entries: Vec<PathBuf> = std::env::split_paths(&path).collect();
  if let Some(index) = entries.iter().position(|entry| entry == wrapper_dir) {
    let directory = entries.remove(index);
    entries.insert(0, directory);
    let rewritten = std::env::join_paths(&entries)
      .context("the spawn PATH entries join with the path separator")?
      .into_string()
      .map_err(|_| anyhow::anyhow!("the spawn PATH is not valid Unicode"))?;
    env.insert("PATH".to_string(), rewritten);
  }
  match resolve_gh(&env["PATH"]) {
    Some(found) if is_same_file(&found, &wrapper_path) => Ok(()),
    Some(found) => bail!(
      "the spawn PATH resolves gh to {}, not the agent gh wrapper at {wrapper}; refusing to start the agent process",
      found.display()
    ),
    None => bail!("no gh on the spawn PATH; the agent gh wrapper is at {wrapper}"),
  }
}

/// The first executable `gh` on a PATH value, or `None` when no entry holds one.
fn resolve_gh(path: &str) -> Option<PathBuf> {
  std::env::split_paths(path).map(|dir| dir.join("gh")).find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
  fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn is_same_file(found: &Path, wrapper: &Path) -> bool {
  fs::canonicalize(found)
    .is_ok_and(|found| found == fs::canonicalize(wrapper).unwrap_or_else(|_| wrapper.to_path_buf()))
}

#[cfg(test)]
mod tests {
  use std::process::Command;

  use super::*;

  fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
  }

  /// The wrapper script of this checkout, as the tests run it for real.
  fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../").join(WRAPPER_REPO_DIR).join("gh")
  }

  fn env_with(pairs: &[(&str, String)]) -> Env {
    pairs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect()
  }

  // --- spawn check ---

  #[test]
  fn spawn_check_passes_when_path_resolves_the_wrapper() {
    let root = tempfile::tempdir().unwrap();
    let wrapper_dir = root.path().join("wrapper");
    fs::create_dir(&wrapper_dir).unwrap();
    write_executable(&wrapper_dir.join("gh"), "#!/bin/sh\n");
    let mut env = env_with(&[
      (WRAPPER_PATH_VAR, wrapper_dir.join("gh").display().to_string()),
      ("PATH", format!("{}:/bin", wrapper_dir.display())),
    ]);

    require_gh_wrapper(&mut env).unwrap();
  }

  /// A backend that prepends a directory holding another `gh` (the personal `~/.local/bin`) must not win: the
  /// wrapper directory goes back to the front and the other entries keep their order.
  #[test]
  fn spawn_check_moves_the_wrapper_directory_back_to_the_front() {
    let root = tempfile::tempdir().unwrap();
    let wrapper_dir = root.path().join("wrapper");
    let other_dir = root.path().join("other");
    fs::create_dir(&wrapper_dir).unwrap();
    fs::create_dir(&other_dir).unwrap();
    write_executable(&wrapper_dir.join("gh"), "#!/bin/sh\n");
    write_executable(&other_dir.join("gh"), "#!/bin/sh\n");
    let mut env = env_with(&[
      (WRAPPER_PATH_VAR, wrapper_dir.join("gh").display().to_string()),
      ("PATH", format!("{}:{}:/bin", other_dir.display(), wrapper_dir.display())),
    ]);

    require_gh_wrapper(&mut env).unwrap();

    let entries: Vec<PathBuf> = std::env::split_paths(&env["PATH"]).collect();
    assert_eq!(entries, [wrapper_dir, other_dir, PathBuf::from("/bin")]);
  }

  #[test]
  fn spawn_check_names_the_resolved_path_when_the_wrapper_is_not_on_path() {
    let root = tempfile::tempdir().unwrap();
    let wrapper_dir = root.path().join("wrapper");
    let other_dir = root.path().join("other");
    fs::create_dir(&wrapper_dir).unwrap();
    fs::create_dir(&other_dir).unwrap();
    write_executable(&wrapper_dir.join("gh"), "#!/bin/sh\n");
    write_executable(&other_dir.join("gh"), "#!/bin/sh\n");
    let mut env = env_with(&[
      (WRAPPER_PATH_VAR, wrapper_dir.join("gh").display().to_string()),
      ("PATH", format!("{}:/bin", other_dir.display())),
    ]);

    let error = require_gh_wrapper(&mut env).unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains(&other_dir.join("gh").display().to_string()), "{message}");
    assert!(message.contains(&wrapper_dir.join("gh").display().to_string()), "{message}");
  }

  #[test]
  fn spawn_check_names_the_wrapper_when_no_gh_resolves() {
    let mut env = env_with(&[(WRAPPER_PATH_VAR, "/synthetic/wrapper/gh".to_string()), ("PATH", "/bin".to_string())]);

    let error = require_gh_wrapper(&mut env).unwrap_err();

    assert!(format!("{error:#}").contains("/synthetic/wrapper/gh"), "{error:#}");
  }

  #[test]
  fn spawn_check_fails_without_the_wrapper_variable() {
    let mut env = env_with(&[("PATH", "/home/operator/.local/bin:/bin".to_string())]);

    let error = require_gh_wrapper(&mut env).unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains(WRAPPER_PATH_VAR), "{message}");
    assert!(message.contains("refusing to start"), "{message}");
  }

  // --- token command ---

  #[test]
  fn apply_token_command_sets_and_clears_the_agent_variable() {
    let mut env = Env::from([("KEPT".to_string(), "value".to_string())]);

    apply_token_command(&mut env, Some("/synthetic/app-token-command"));
    assert_eq!(env[TOKEN_COMMAND_VAR], "/synthetic/app-token-command");

    apply_token_command(&mut env, None);
    assert!(!env.contains_key(TOKEN_COMMAND_VAR));

    env.insert(TOKEN_COMMAND_VAR.to_string(), "/synthetic/stale".to_string());
    apply_token_command(&mut env, Some(""));
    assert!(!env.contains_key(TOKEN_COMMAND_VAR));
    assert_eq!(env["KEPT"], "value");
  }

  // --- wrapper script classification ---

  /// One stub environment: a real `gh` that records its argv and `GH_TOKEN` (and resolves the current directory to
  /// `stub-owner/stub-repo`), and a token command whose exit the test sets.
  struct Stubs {
    root: tempfile::TempDir,
    record: PathBuf,
    token_record: PathBuf,
    bin: PathBuf,
  }

  impl Stubs {
    fn start() -> Self {
      let root = tempfile::tempdir().unwrap();
      let bin = root.path().join("bin");
      fs::create_dir(&bin).unwrap();
      let record = root.path().join("gh.record");
      let token_record = root.path().join("token.record");
      write_executable(
        &bin.join("gh"),
        &format!(
          r#"#!/bin/sh
if [ "$1 $2 $3 $4 $5 $6" = 'repo view --json nameWithOwner -q .nameWithOwner' ]; then
  printf 'stub-owner/stub-repo\n'
  exit 0
fi
for a in "$@"; do
  printf 'argv:%s\n' "$a" >> '{record}'
done
printf 'token:%s\n' "${{GH_TOKEN-}}" >> '{record}'
"#,
          record = record.display()
        ),
      );
      write_executable(
        &bin.join("token-cmd"),
        &format!(
          r#"#!/bin/sh
for a in "$@"; do
  printf 'argv:%s\n' "$a" >> '{token_record}'
done
case ${{TOKEN_EXIT:-0}} in
  0) printf 'stub-app-token\n' ;;
  3) exit 3 ;;
  *) printf 'token boom\n' >&2; exit 7 ;;
esac
"#,
          token_record = token_record.display()
        ),
      );
      Self { root, record, token_record, bin }
    }

    /// The test process's own GitHub credentials stay out of the wrapper's environment, so a run sees only what the
    /// test sets.
    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> std::process::Output {
      let mut command = Command::new(script());
      command
        .args(args)
        .current_dir(self.root.path())
        .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env(TOKEN_COMMAND_VAR, self.bin.join("token-cmd"));
      for (name, value) in extra {
        command.env(name, value);
      }
      command.output().expect("the wrapper script runs")
    }

    /// The same call with the token command unset, as the server leaves it without the config key.
    fn run_without_token_command(&self, args: &[&str]) -> std::process::Output {
      let mut command = Command::new(script());
      command
        .args(args)
        .current_dir(self.root.path())
        .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove(TOKEN_COMMAND_VAR);
      command.output().expect("the wrapper script runs")
    }

    fn run_agent_gh_without_token_command(&self, args: &[&str], initial_path: &str) -> (std::process::Output, String) {
      let wrapper = script();
      let mut environment =
        env_with(&[(WRAPPER_PATH_VAR, wrapper.display().to_string()), ("PATH", initial_path.to_string())]);
      require_gh_wrapper(&mut environment).unwrap();
      let path = environment["PATH"].clone();
      let agent_gh = resolve_gh(&path).expect("the agent PATH resolves gh");
      let output = Command::new(agent_gh)
        .args(args)
        .current_dir(self.root.path())
        .envs(&environment)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove(TOKEN_COMMAND_VAR)
        .output()
        .expect("the agent gh wrapper runs");
      (output, path)
    }

    fn record(&self) -> String {
      fs::read_to_string(&self.record).unwrap_or_default()
    }

    fn token_record(&self) -> String {
      fs::read_to_string(&self.token_record).unwrap_or_default()
    }
  }

  fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
  }

  /// Claude Code's Haiku environment leaves PATH as built; Codex prepends its local bin. Both go through the same
  /// runner spawn check before invoking `gh`.
  #[test]
  #[ignore = "integration"]
  fn agent_gh_uses_the_host_login_without_an_app_command_for_haiku_and_codex() {
    let wrapper_dir = script().parent().unwrap().to_path_buf();
    for (backend, prepends_local_bin) in [("haiku", false), ("codex", true)] {
      let stubs = Stubs::start();
      let initial_path = if prepends_local_bin {
        format!("{}:{}:/usr/bin:/bin", stubs.bin.display(), wrapper_dir.display())
      } else {
        format!("{}:{}:/usr/bin:/bin", wrapper_dir.display(), stubs.bin.display())
      };

      let (output, path) =
        stubs.run_agent_gh_without_token_command(&["pr", "create", "-R", "o/r", "--title", "t"], &initial_path);

      assert!(output.status.success(), "{backend}: {}", String::from_utf8_lossy(&output.stderr));
      let entries: Vec<PathBuf> = std::env::split_paths(&path).collect();
      assert_eq!(entries[0], wrapper_dir, "{backend}");
      assert!(stubs.token_record().is_empty(), "{backend}: {}", stubs.token_record());
      let record = stubs.record();
      assert!(record.contains("token:\n"), "{backend}: {record}");
      for arg in ["pr", "create", "-R", "o/r", "--title", "t"] {
        assert!(record.contains(&format!("argv:{arg}\n")), "{backend}: {record}");
      }
    }
  }

  #[test]
  #[ignore = "integration"]
  fn agent_gh_uses_the_configured_app_token_for_text_writes() {
    let stubs = Stubs::start();

    let output = stubs.run(&["pr", "create", "-R", "o/r", "--title", "t"], &[("TOKEN_EXIT", "0")]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(stubs.token_record(), "argv:--write\nargv:o/r\n");
    assert!(stubs.record().contains("token:stub-app-token\n"), "{}", stubs.record());
  }

  /// The text-carrying forms: each one mints the App token for its target and runs gh with `GH_TOKEN` set. The
  /// original argv reaches the real gh unchanged.
  #[test]
  #[ignore = "integration"]
  fn text_writes_mint_an_app_token_for_their_target() {
    let cases: Vec<(Vec<&str>, &str)> = vec![
      (vec!["pr", "create", "-R", "o/r", "--title", "t"], "o/r"),
      (vec!["pr", "new", "-R", "o/r", "--title", "t"], "o/r"),
      (vec!["issue", "new", "-R", "o/r", "--title", "t"], "o/r"),
      (vec!["pr", "edit", "-R", "o/r", "12", "--body", "b"], "o/r"),
      (vec!["pr", "comment", "-R", "o/r", "12", "--body-file", "f"], "o/r"),
      (vec!["pr", "review", "-R", "o/r", "12", "--approve"], "o/r"),
      (vec!["pr", "close", "-R", "o/r", "12", "-c", "done"], "o/r"),
      (vec!["pr", "close", "-R", "o/r", "12", "--comment=done"], "o/r"),
      (vec!["pr", "reopen", "-R", "o/r", "12", "-c", "reopening"], "o/r"),
      (vec!["issue", "close", "-R", "o/r", "3", "-c", "closing"], "o/r"),
      (vec!["issue", "reopen", "-R", "o/r", "3", "--comment=reopening"], "o/r"),
      (vec!["pr", "merge", "-R", "o/r", "12", "-b", "subject line"], "o/r"),
      (vec!["pr", "merge", "-R", "o/r", "12", "--subject=squash title"], "o/r"),
      (vec!["pr", "merge", "-R", "o/r", "12", "-F", "body.md"], "o/r"),
      (vec!["pr", "merge", "-R", "o/r", "12", "--body-file=body.md"], "o/r"),
      (vec!["release", "create", "-R", "o/r", "v1", "-n", "release notes"], "o/r"),
      (vec!["release", "create", "-R", "o/r", "v1", "--notes=release notes"], "o/r"),
      (vec!["release", "create", "-R", "o/r", "v1", "-F", "notes.md"], "o/r"),
      (vec!["release", "create", "-R", "o/r", "v1", "--notes-file=notes.md"], "o/r"),
      (vec!["release", "create", "-R", "o/r", "v1", "--generate-notes"], "o/r"),
      (vec!["release", "create", "-R", "o/r", "v1", "--notes-from-tag"], "o/r"),
      (vec!["release", "create", "-R", "o/r", "v1", "-t", "release title"], "o/r"),
      (vec!["release", "edit", "-R", "o/r", "v1", "-n", "release notes"], "o/r"),
      (vec!["release", "edit", "-R", "o/r", "v1", "-F", "notes.md"], "o/r"),
      (vec!["discussion", "create", "-R", "o/r", "-t", "topic", "-b", "body"], "o/r"),
      (vec!["discussion", "create", "-R", "o/r", "-F", "body.md"], "o/r"),
      (vec!["discussion", "edit", "-R", "o/r", "12", "--title=topic"], "o/r"),
      (vec!["discussion", "comment", "-R", "o/r", "12", "-b", "reply"], "o/r"),
      (vec!["discussion", "comment", "-R", "o/r", "12", "--body-file=reply.md"], "o/r"),
      (vec!["issue", "create", "-R", "o/r", "--title", "t"], "o/r"),
      (vec!["issue", "edit", "-R", "o/r", "3", "--add-assignee", "@me"], "o/r"),
      (vec!["issue", "comment", "-R", "o/r", "3", "-b", "text"], "o/r"),
      (vec!["api", "-f", "k=v", "repos/o/r/issues"], "o/r"),
      (vec!["api", "-X", "POST", "repos/o/r/issues/1"], "o/r"),
      (vec!["api", "--method=PUT", "repos/o/r/labels/x"], "o/r"),
      (vec!["api", "-XDELETE", "https://api.github.com/repos/o/r/issues/1"], "o/r"),
      (vec!["api", "-X", "POST", "repos/{owner}/{repo}/issues"], "stub-owner/stub-repo"),
      (vec!["api", "--silent", "-f", "k=v", "repos/o/r/issues"], "o/r"),
      (vec!["api", "-q", ".id", "-f", "k=v", "repos/o/r/issues"], "o/r"),
      (vec!["api", "--verbose", "-X", "PATCH", "-p", "preview", "repos/o/r/issues/1"], "o/r"),
      (vec!["api", "-X", "POST", "repos/o/r?per_page=1"], "o/r"),
      (vec!["pr", "merge", "-R", "o/r", "12", "-db", "subject line"], "o/r"),
      (vec!["pr", "close", "-R", "o/r", "12", "-dc", "done"], "o/r"),
      (vec!["api", "-iXDELETE", "repos/o/r/git/refs/heads/x"], "o/r"),
    ];

    for (args, target) in &cases {
      let stubs = Stubs::start();
      let output = stubs.run(args, &[("TOKEN_EXIT", "0")]);

      assert!(output.status.success(), "{args:?}: {}", stdout_of(&output));
      assert_eq!(stubs.token_record(), format!("argv:--write\nargv:{target}\n"), "{args:?}");
      let record = stubs.record();
      assert!(record.contains("token:stub-app-token\n"), "{args:?}: {record}");
      for arg in args {
        assert!(record.contains(&format!("argv:{arg}\n")), "{args:?}: {record}");
      }
    }
  }

  /// The passthrough forms: no token lookup, no `GH_TOKEN`, argv unchanged.
  #[test]
  #[ignore = "integration"]
  fn every_other_call_passes_through_without_a_token_lookup() {
    let cases: Vec<Vec<&str>> = vec![
      vec!["pr", "view", "-R", "o/r", "12"],
      vec!["pr", "close", "-R", "o/r", "12"],
      vec!["pr", "merge", "-R", "o/r", "12", "--squash"],
      vec!["pr", "list", "--state", "open"],
      vec!["issue", "view", "-R", "o/r", "3"],
      vec!["issue", "close", "-R", "o/r", "3"],
      vec!["issue", "reopen", "-R", "o/r", "3"],
      vec!["pr", "reopen", "-R", "o/r", "12"],
      // A release without notes or a title publishes no agent text.
      vec!["release", "create", "-R", "o/r", "v1"],
      vec!["release", "edit", "-R", "o/r", "v1", "--draft=false"],
      // Discussion calls that carry no title or body.
      vec!["discussion", "comment", "-R", "o/r", "12", "--delete"],
      vec!["discussion", "create", "-R", "o/r", "-c", "Announcements"],
      vec!["discussion", "edit", "-R", "o/r", "12", "--add-label", "topic"],
      // Writes whose target is not a repository.
      vec!["gist", "create", "-d", "description", "file.txt"],
      vec!["gist", "edit", "-d", "description", "abc123"],
      vec!["repo", "view"],
      vec!["run", "watch"],
      vec!["api", "repos/o/r/pulls"],
      vec!["api", "--paginate", "repos/o/r/pulls"],
      vec!["api", "-X", "GET", "-f", "k=v", "repos/o/r/issues"],
      vec!["api", "user", "--jq", ".login"],
      vec!["api", "repos/{owner}/{repo}/pulls"],
      // GraphQL documents whose operations are all queries.
      vec!["api", "graphql", "-f", "query={viewer{login}}"],
      vec!["api", "graphql", "-f", "query=query Q($mutation: Int) { viewer { login } }"],
      vec!["api", "graphql", "-F", "query=query { viewer { login } }"],
      vec!["api", "-X", "POST", "graphql", "-f", "query=# mutation in a comment\nquery { viewer { login } }"],
      vec!["api", "-X", "POST", "graphql", "-f", "query=query { search(type: ISSUE, query: \"mutation\") { x } }"],
      vec!["api", "-X", "GET", "graphql", "-f", "query={viewer{login}}"],
      // Bool-only clusters and clusters whose value letter is not a text flag read no text.
      vec!["pr", "merge", "-R", "o/r", "12", "-sd"],
      vec!["api", "-iq", ".x", "repos/o/r/pulls"],
    ];

    for args in &cases {
      let stubs = Stubs::start();
      let output = stubs.run(args, &[]);

      assert!(output.status.success(), "{args:?}: {}", stdout_of(&output));
      assert!(stubs.token_record().is_empty(), "{args:?}: {}", stubs.token_record());
      let record = stubs.record();
      assert!(record.contains("token:\n"), "{args:?}: {record}");
      for arg in args {
        assert!(record.contains(&format!("argv:{arg}\n")), "{args:?}: {record}");
      }
    }
  }

  #[test]
  #[ignore = "integration"]
  fn exit_three_runs_the_personal_login_without_a_token() {
    let stubs = Stubs::start();
    let output = stubs.run(&["pr", "create", "-R", "o/r"], &[("TOKEN_EXIT", "3")]);

    assert!(output.status.success(), "{}", stdout_of(&output));
    assert_eq!(stubs.token_record(), "argv:--write\nargv:o/r\n");
    assert!(stubs.record().contains("token:\n"), "{}", stubs.record());
  }

  /// Each newly classified form keeps today's behavior when the App is not installed on the target: the real gh runs
  /// as the personal login, with no `GH_TOKEN`.
  #[test]
  #[ignore = "integration"]
  fn newly_classified_forms_run_unchanged_when_the_app_is_not_installed() {
    let cases: Vec<Vec<&str>> = vec![
      vec!["issue", "close", "-R", "o/r", "3", "-c", "closing"],
      vec!["issue", "reopen", "-R", "o/r", "3", "-c", "reopening"],
      vec!["pr", "reopen", "-R", "o/r", "12", "-c", "reopening"],
      vec!["pr", "merge", "-R", "o/r", "12", "-F", "body.md"],
      vec!["release", "create", "-R", "o/r", "v1", "-n", "release notes"],
      vec!["release", "create", "-R", "o/r", "v1", "-F", "notes.md"],
      vec!["release", "create", "-R", "o/r", "v1", "--generate-notes"],
      vec!["release", "edit", "-R", "o/r", "v1", "--notes=release notes"],
      vec!["discussion", "create", "-R", "o/r", "-t", "topic", "-b", "body"],
      vec!["discussion", "comment", "-R", "o/r", "12", "-b", "reply"],
    ];

    for args in &cases {
      let stubs = Stubs::start();
      let output = stubs.run(args, &[("TOKEN_EXIT", "3")]);

      assert!(output.status.success(), "{args:?}: {}", stdout_of(&output));
      assert_eq!(stubs.token_record(), "argv:--write\nargv:o/r\n", "{args:?}");
      assert!(stubs.record().contains("token:\n"), "{args:?}: {}", stubs.record());
      for arg in args {
        assert!(stubs.record().contains(&format!("argv:{arg}\n")), "{args:?}: {}", stubs.record());
      }
    }
  }

  /// A write whose target is not a repository never looks up the App token.
  #[test]
  #[ignore = "integration"]
  fn non_repository_writes_pass_through() {
    for args in [
      vec!["gist", "create", "-d", "description", "file.txt"],
      vec!["gist", "edit", "-d", "description", "abc123"],
      vec!["gist", "create", "file.txt"],
    ] {
      let stubs = Stubs::start();
      let output = stubs.run(&args, &[]);

      assert!(output.status.success(), "{args:?}: {}", stdout_of(&output));
      assert!(stubs.token_record().is_empty(), "{args:?}");
      assert!(stubs.record().contains("token:\n"), "{args:?}: {}", stubs.record());
    }
  }

  #[test]
  #[ignore = "integration"]
  fn a_token_error_runs_nothing() {
    let stubs = Stubs::start();
    let output = stubs.run(&["pr", "create", "-R", "o/r"], &[("TOKEN_EXIT", "7")]);

    assert_eq!(output.status.code(), Some(7));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("token boom"), "{stderr}");
    assert!(stderr.contains("nothing was run"), "{stderr}");
    assert!(stubs.record().is_empty(), "{}", stubs.record());
  }

  #[test]
  #[ignore = "integration"]
  fn an_unresolvable_target_is_refused_before_anything_runs() {
    for args in [
      vec!["api", "graphql", "-f", "query=mutation{x}"],
      vec!["api", "-X", "POST", "/user"],
      vec!["api", "-f", "k=v"],
      vec!["pr", "create", "-R", "github.com/o/r", "--title", "t"],
      vec!["pr", "comment", "-R", "o", "12", "-b", "x"],
    ] {
      let stubs = Stubs::start();
      let output = stubs.run(&args, &[]);

      assert_eq!(output.status.code(), Some(1), "{args:?}");
      let stderr = String::from_utf8_lossy(&output.stderr);
      assert!(stderr.contains("-R"), "{args:?}: {stderr}");
      assert!(stderr.contains("repos/<owner>/<repo>"), "{args:?}: {stderr}");
      assert!(stubs.record().is_empty(), "{args:?}");
      assert!(stubs.token_record().is_empty(), "{args:?}");
    }
  }

  /// A GraphQL document with a mutation or subscription operation, or one whose operation type cannot be read, is
  /// refused before anything runs.
  #[test]
  #[ignore = "integration"]
  fn graphql_documents_that_are_not_query_only_are_refused() {
    let cases: Vec<Vec<&str>> = vec![
      vec!["api", "graphql", "-f", "query=mutation { x }"],
      vec!["api", "-X", "POST", "graphql", "-f", "query=subscription { x }"],
      vec!["api", "graphql", "-f", "query=query { a } mutation { b }"],
      vec!["api", "graphql", "-X", "POST"],
      vec!["api", "graphql", "--input", "body.json"],
      vec!["api", "graphql", "-f", "query=query { a }", "--input", "body.json"],
      vec!["api", "graphql", "-F", "query=@/synthetic/unreadable-query.graphql"],
      vec!["api", "-X", "GET", "graphql", "-f", "query=mutation { x }"],
      // Two query fields: gh's order for them is not read, so the document is unknown whichever one is a mutation.
      vec!["api", "graphql", "-f", "query={viewer{login}}", "-F", "query=@/synthetic/mutation.graphql"],
      vec!["api", "graphql", "-f", "query={viewer{login}}", "-f", "query={viewer{name}}"],
    ];

    for args in &cases {
      let stubs = Stubs::start();
      let output = stubs.run(args, &[]);

      assert_eq!(output.status.code(), Some(1), "{args:?}");
      let stderr = String::from_utf8_lossy(&output.stderr);
      assert!(stderr.contains("repos/<owner>/<repo>"), "{args:?}: {stderr}");
      assert!(stubs.record().is_empty(), "{args:?}");
      assert!(stubs.token_record().is_empty(), "{args:?}");
    }
  }

  /// A query document in an @file is read like one passed inline.
  #[test]
  #[ignore = "integration"]
  fn a_graphql_query_in_a_file_passes_through() {
    let stubs = Stubs::start();
    let query_file = stubs.root.path().join("query.graphql");
    fs::write(&query_file, "query Viewer {\n  viewer { login }\n}\n").unwrap();
    let argument = format!("query=@{}", query_file.display());

    let output = stubs.run(&["api", "graphql", "-F", &argument], &[]);

    assert!(output.status.success(), "{}", stdout_of(&output));
    assert!(stubs.token_record().is_empty(), "{}", stubs.token_record());
    assert!(stubs.record().contains(&format!("argv:{argument}\n")), "{}", stubs.record());
  }

  /// A PATH entry that reaches the wrapper's own directory through a symlink must not make the wrapper exec itself:
  /// the search skips that directory and takes the next real gh.
  #[test]
  #[ignore = "integration"]
  fn an_aliased_wrapper_directory_is_skipped_rather_than_executed() {
    let stubs = Stubs::start();
    let alias = stubs.root.path().join("wrapper-alias");
    std::os::unix::fs::symlink(script().parent().unwrap(), &alias).unwrap();

    let output = Command::new(script())
      .args(["pr", "view", "-R", "o/r", "12"])
      .current_dir(stubs.root.path())
      .env("PATH", format!("{}:{}:/usr/bin:/bin", alias.display(), stubs.bin.display()))
      .env(TOKEN_COMMAND_VAR, stubs.bin.join("token-cmd"))
      .output()
      .expect("the wrapper script runs");

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(stubs.record().contains("argv:pr\n"), "the real gh ran: {}", stubs.record());
  }

  #[test]
  #[ignore = "integration"]
  fn text_writes_without_the_token_command_use_the_host_login() {
    for args in [vec!["pr", "create", "-R", "o/r"], vec!["api", "-f", "k=v", "repos/o/r/issues"]] {
      let stubs = Stubs::start();
      let output = stubs.run_without_token_command(&args);

      assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
      assert!(stubs.record().contains("token:\n"), "{args:?}: {}", stubs.record());
      assert!(stubs.token_record().is_empty(), "{args:?}");
    }
  }

  #[test]
  #[ignore = "integration"]
  fn a_pass_through_call_needs_no_token_command() {
    for args in [vec!["pr", "view", "-R", "o/r", "12"], vec!["api", "graphql", "-f", "query={viewer{login}}"]] {
      let stubs = Stubs::start();
      let output = stubs.run_without_token_command(&args);

      assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
      assert!(stubs.token_record().is_empty(), "{args:?}");
    }
  }

  #[test]
  #[ignore = "integration"]
  fn without_r_the_target_comes_from_the_current_directory() {
    let stubs = Stubs::start();
    let output = stubs.run(&["pr", "create", "--title", "t"], &[]);

    assert!(output.status.success(), "{}", stdout_of(&output));
    assert!(stubs.token_record().contains("argv:--write\nargv:stub-owner/stub-repo\n"), "{}", stubs.token_record());
  }

  #[test]
  #[ignore = "integration"]
  fn a_failed_cwd_resolution_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_executable(&bin.join("gh"), "#!/bin/sh\nexit 4\n");
    write_executable(&bin.join("token-cmd"), "#!/bin/sh\nprintf 'stub-app-token\\n'\n");
    let output = Command::new(script())
      .args(["pr", "create", "--title", "t"])
      .current_dir(root.path())
      .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
      .env("CHARLIEBOT_GH_APP_TOKEN_CMD", bin.join("token-cmd"))
      .output()
      .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("-R"), "{stderr}");
  }
}
