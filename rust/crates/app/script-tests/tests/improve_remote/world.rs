//! The improve round's test world: a throwaway repository that holds the launcher, a profile home, a remote home and
//! the stub commands of both sides. It also holds a local `claude` stub for a cron session's own turn.
//!
//! The repository is a real git repository with `origin/main`, so the launcher reads its commit and
//! `dev/improve/inflight-files.sh` lists this repository's own worktrees only. The stub `ssh` records its arguments
//! and then runs the remote command locally under `SSH_REMOTE_HOME` with a clean environment. That environment
//! carries the `FAKE_` variables the stubs read. A real ssh sends no other variable, so a variable that the launcher
//! fails to forward is absent on the remote side here too. Like a real ssh, the stub sends its standard input to the
//! remote command unless `-n` is given, and the remote command may drop what it never reads.
//!
//! Every path this world hands out is under its own `mkdtemp` sandbox, and the sandbox removes it when it drops. A
//! test that needs an absent input removes that one fixture path inside the sandbox. No test removes the profile home,
//! and no delete names a path outside the sandbox.

use std::fs;
use std::path::{Path, PathBuf};

use charliebot_shell_harness::{self as harness, Env, Sandbox, write_executable};

/// The committer identity of every fixture commit.
const IDENTITY: [(&str, &str); 4] = [
  ("GIT_AUTHOR_NAME", "improve-test"),
  ("GIT_AUTHOR_EMAIL", "improve-test@example.invalid"),
  ("GIT_COMMITTER_NAME", "improve-test"),
  ("GIT_COMMITTER_EMAIL", "improve-test@example.invalid"),
];

/// The direction of every test round.
pub const DIRECTION: &str = "perf";
/// The GitHub origin of the launcher's checkout, as a live host's checkout names it.
pub const GITHUB_ORIGIN: &str = "https://github.com/fixture-owner/fixture-repo.git";
/// The repository that origin names, as the GitHub CLI writes it.
pub const GITHUB_REPOSITORY: &str = "github.com/fixture-owner/fixture-repo";
/// A round worker: a child of the cron session, so its id names no cron task file.
pub const SESSION_ID: &str = "11111111-2222-3333-4444-555555555555";
/// The cron session: the task file binds it as `session_id`, and the round worker calls it its parent.
pub const PARENT_ID: &str = "99999999-8888-7777-6666-555555555555";
pub const GATEWAY_TOKEN: &str = "synthetic-gateway-token";
pub const API_KEY: &str = "synthetic-api-key";
pub const RUN_TOKEN: &str = "synthetic-run-token";
pub const REMOTE_HOST: &str = "improve.example.ts.net";
pub const MOCK_HOME_VERSION: &str = "2";
pub const LOCAL_CONFIG_DIR: &str = "/synthetic/local-claude-config";
pub const INSTRUCTIONS: &str = "Improve one topic.\n";

const SSH: &str = r#"# Fake ssh: record every argument, then run the remote command locally under the remote home.
record='--- call'$'\n'
for argument in "$@"; do
  record+="$argument"$'\n'
done
printf '%s' "$record" >>"$SSH_ARG_LOG"
remote=
for last in "$@"; do
  remote=$last
done
if [[ -n ${SSH_APPROVAL_FILE:-} && -e $SSH_APPROVAL_FILE ]]; then
  printf 'To approve this connection, open https://login.example.ts.net/a/synthetic-approval\n' >&2
  sleep 30
  exit 255
fi
if [[ ${SSH_FAIL_EVIDENCE_LISTING:-0} == 1 && $remote == *'-mindepth 1 -printf'* ]]; then
  printf 'stub ssh: the evidence listing failed\n' >&2
  exit 7
fi
if [[ ${SSH_HANG_TAR:-0} == 1 && $remote == *'tar '*'-cf -'* ]]; then
  sleep 30
fi
environment=()
while IFS= read -r name; do
  case $name in
    FAKE_*) environment+=("$name=${!name}") ;;
  esac
done < <(compgen -e)
forward=1
for argument in "$@"; do
  [[ $argument == -n ]] && forward=0
done
if [[ $forward == 1 ]]; then
  cat | env -i "HOME=$SSH_REMOTE_HOME" "PATH=$SSH_REMOTE_PATH" "${environment[@]}" bash -c "$remote"
else
  exec env -i "HOME=$SSH_REMOTE_HOME" "PATH=$SSH_REMOTE_PATH" "${environment[@]}" bash -c "$remote" </dev/null
fi
"#;

const CLAUDE: &str = r#"# Fake claude: record the arguments and environment it receives, then exit as the round asks.
if [[ -n ${IMPROVE_DIRECTION:-} ]]; then
  if [[ ! -s $FAKE_MOCK_HOME_DIR/manifest.json ]]; then
    printf 'the shared mock home is missing its manifest before the round starts\n' >&2
    exit 91
  fi
  printf 'round %s\n' "$IMPROVE_DIRECTION" >>"$FAKE_MOCK_HOME_LOG"
  if [[ -n ${FAKE_CLAUDE_BARRIER:-} ]]; then
    : >"$FAKE_CLAUDE_BARRIER/$IMPROVE_DIRECTION"
    for _ in $(seq 1 600); do
      [[ -e $FAKE_CLAUDE_BARRIER/perf && -e $FAKE_CLAUDE_BARRIER/ux ]] && break
      sleep 0.1
    done
    [[ -e $FAKE_CLAUDE_BARRIER/perf && -e $FAKE_CLAUDE_BARRIER/ux ]] || {
      printf 'both rounds did not reach the shared mock home barrier\n' >&2
      exit 92
    }
  fi
  if [[ ! -s $FAKE_MOCK_HOME_DIR/manifest.json ]]; then
    printf 'the shared mock home changed while the round was running\n' >&2
    exit 93
  fi
fi
printf '%s\n' "$@" >"$FAKE_CLAUDE_ARGV"
# The record keeps the gateway variables out of the evidence: a real claude holds its environment in memory only, so
# the record carries each variable's digest in its place.
env | grep -v -e '^ANTHROPIC_AUTH_TOKEN=' -e '^CHARLIE_CODE_API_KEY=' >"$FAKE_CLAUDE_ENV"
printf 'ANTHROPIC_AUTH_TOKEN_SHA256=%s\nCHARLIE_CODE_API_KEY_SHA256=%s\n' \
  "$(printf '%s' "${ANTHROPIC_AUTH_TOKEN-}" | sha256sum | cut -d' ' -f1)" \
  "$(printf '%s' "${CHARLIE_CODE_API_KEY-}" | sha256sum | cut -d' ' -f1)" \
  >>"$FAKE_CLAUDE_ENV"
if [[ -n ${FAKE_CLAUDE_STDIN:-} ]]; then
  cat >"$FAKE_CLAUDE_STDIN"
fi
if [[ -n ${FAKE_CLAUDE_INFLIGHT:-} && -e $IMPROVE_LAUNCH_DIR/inflight-files.txt ]]; then
  cp -- "$IMPROVE_LAUNCH_DIR/inflight-files.txt" "$FAKE_CLAUDE_INFLIGHT"
fi
if [[ -n ${FAKE_CLAUDE_WAIT_FOR:-} ]]; then
  for _ in $(seq 1 600); do
    [[ -e $FAKE_CLAUDE_WAIT_FOR ]] && break
    sleep 0.1
  done
fi
exit "${FAKE_CLAUDE_EXIT:-0}"
"#;

const MOCK_HOME: &str = r#"# Fake mock-home: record its calls and write a manifest as the completion marker.
case ${1-} in
  version)
    printf 'version\n' >>"$FAKE_MOCK_HOME_LOG"
    printf '%s\n' '2'
    ;;
  generate)
    printf 'generate %s\n' "$2" >>"$FAKE_MOCK_HOME_LOG"
    if [[ ${FAKE_MOCK_HOME_FAIL:-0} == 1 ]]; then
      printf 'synthetic mock-home generation failure: disk full\n' >&2
      exit 23
    fi
    mkdir -p -- "$2"
    printf 'synthetic complete mock home\n' >"$2/manifest.json"
    ;;
  *)
    printf 'unexpected mock-home command: %s\n' "${1-}" >&2
    exit 2
    ;;
esac
"#;

const SYSTEMD_RUN: &str = r#"# Fake systemd-run: record the call, then run the command after `--`. An already-loaded unit
# refuses the start, unless a fake systemctl stop has ended that unit first.
printf '%s\n' "$*" >>"$FAKE_SYSTEMD_RUN_LOG"
if [[ ${FAKE_SYSTEMD_RUN_EXISTING:-0} == 1 && ! -e $FAKE_SCOPE_STOPPED ]]; then
  printf 'Failed to start transient scope unit: Unit improve-perf.scope was already loaded or has a fragment file.\n' >&2
  exit 1
fi
arguments=("$@")
separator=0
index=0
for argument in "$@"; do
  if [[ $argument == -- ]]; then
    separator=$index
  fi
  index=$((index + 1))
done
exec "${arguments[@]:separator+1}"
"#;

const SYSTEMCTL: &str = r#"# Fake systemctl: record the call, answer is-failed and is-active from the scope's state, and mark
# the scope stopped.
printf '%s\n' "$*" >>"$FAKE_SYSTEMCTL_LOG"
case $* in
  *"is-failed"*)
    case ${FAKE_SCOPE_STATE:-absent} in
      failed) exit 0 ;;
      absent) exit 4 ;;
    esac
    exit 1
    ;;
  *"is-active"*)
    [[ ${FAKE_SCOPE_STATE:-} == active ]] && exit 0
    exit 3
    ;;
  *"stop improve-"*) : >"$FAKE_SCOPE_STOPPED" ;;
esac
exit 0
"#;

const SANDBOX: &str = r#"# Fake improve-sandbox: no boundary. It runs the command after `--`, as the real program does.
shift
[[ ${1-} == -- ]] || exit 1
shift
exec "$@"
"#;

const CARGO: &str = r#"# Fake cargo: record the build environment, then write the release tools where cargo puts them.
printf 'CARGO_HOME=%s\nTMPDIR=%s\n' "$CARGO_HOME" "$TMPDIR" >"$FAKE_CARGO_ENV"
if [[ -n ${FAKE_CARGO_LOG:-} ]]; then
  printf 'build\n' >>"$FAKE_CARGO_LOG"
fi
if [[ -n ${FAKE_CARGO_SLEEP:-} ]]; then
  sleep "$FAKE_CARGO_SLEEP"
fi
mkdir -p "$CARGO_TARGET_DIR/release"
printf '#!/usr/bin/env bash\nexit 0\n' >"$CARGO_TARGET_DIR/release/charliebot"
cp -- "$FAKE_MOCK_HOME_FIXTURE" "$CARGO_TARGET_DIR/release/mock-home"
# The built improve-sandbox runs the command after `--`, as the real program does.
printf '#!/usr/bin/env bash\nshift\n[[ ${1-} == -- ]] || exit 1\nshift\nexec "$@"\n' \
  >"$CARGO_TARGET_DIR/release/improve-sandbox"
chmod 755 "$CARGO_TARGET_DIR/release/charliebot" "$CARGO_TARGET_DIR/release/mock-home" \
  "$CARGO_TARGET_DIR/release/improve-sandbox"
"#;

const GH: &str = r#"# Fake gh: answer from the fixture that FAKE_GH_PRS names, one line per open pull request: number, head, file.
if [[ -z ${FAKE_GH_PRS:-} || ! -r $FAKE_GH_PRS ]]; then
  exit 0
fi
case " $* " in
  *" pr list "*)
    if [[ -n ${FAKE_GH_REPO:-} && " $* " != *" --repo $FAKE_GH_REPO "* ]]; then
      printf 'stub gh: pull request list did not name the expected repository\n' >&2
      exit 91
    fi
    while IFS=$'\t' read -r number head file; do
      printf '%s\t%s\n' "$number" "$head"
    done <"$FAKE_GH_PRS"
    ;;
  *" api "*)
    if [[ -n ${FAKE_GH_REPO:-} ]]; then
      arguments=("$@")
      hostname=
      route=
      for ((index = 0; index < ${#arguments[@]}; index++)); do
        if [[ ${arguments[index]} == --hostname ]]; then
          hostname=${arguments[index + 1]}
        elif [[ ${arguments[index]} == repos/* ]]; then
          route=${arguments[index]}
        fi
      done
      repository=${FAKE_GH_REPO#*/}
      if [[ $hostname != ${FAKE_GH_HOST:-} || $route != "repos/$repository/"* ]]; then
        printf 'stub gh: pull request files request did not name the expected repository\n' >&2
        exit 92
      fi
    fi
    if [[ ${FAKE_GH_FAIL_FILES:-0} == 1 ]]; then
      exit 1
    fi
    number=
    for argument in "$@"; do
      case $argument in
        *"/pulls/"*"/files"*)
          number=${argument#*/pulls/}
          number=${number%/files}
          break
          ;;
      esac
    done
    while IFS=$'\t' read -r pr head file; do
      [[ $pr == "$number" ]] && printf '%s\n' "$file"
    done <"$FAKE_GH_PRS"
    ;;
esac
exit 0
"#;

/// One test's repository, both homes and stub directory.
pub struct World {
  pub sandbox: Sandbox,
  /// The launcher's checkout, with its own `origin/main`.
  pub repo: PathBuf,
  /// The commit that the launcher's checkout holds.
  pub commit: String,
  /// `$HOME` of this host.
  pub local_home: PathBuf,
  /// `$HOME` that the fake ssh gives the remote command.
  pub remote_home: PathBuf,
  /// `$CHARLIEBOT_HOME` of this host.
  pub profile_home: PathBuf,
  pub instructions: PathBuf,
  pub ssh_args: PathBuf,
  /// Where the fake claude writes its arguments and environment, inside the round's boundary.
  pub claude_argv: PathBuf,
  pub claude_env: PathBuf,
  /// Where the fake claude copies the round's in-flight files list, inside the round's boundary.
  pub claude_inflight: PathBuf,
  /// Where the fake claude copies its standard input when a test asks for it, inside the round's boundary.
  pub claude_stdin: PathBuf,
  pub systemctl_log: PathBuf,
  pub systemd_run_log: PathBuf,
  pub scope_stopped: PathBuf,
  pub mock_home_log: PathBuf,
  pub mock_home_fixture: PathBuf,
  /// Where the local claude of a cron session's own turn writes its arguments and environment: this
  /// host's records, outside the round's boundary.
  pub node_claude_argv: PathBuf,
  pub node_claude_env: PathBuf,
  /// Where the local claude copies its standard input when a test asks for it.
  pub node_claude_stdin: PathBuf,
  /// The in-memory `$CLAUDE_CONFIG_DIR` of the launcher; it must not reach the round.
  pub local_config_dir: &'static str,
}

impl World {
  pub fn new() -> Self {
    let sandbox = Sandbox::new();
    let repo = sandbox.dir("repo");
    let scripts = sandbox.dir("repo/dev/improve");
    for name in ["remote-launch.sh", "remote-runner.sh", "inflight-files.sh"] {
      harness::copy_file(&harness::repo_root().join("dev/improve").join(name), &scripts.join(name));
    }
    let local_home = sandbox.dir("local");
    let remote_home = sandbox.dir("remote");
    let profile_home = sandbox.dir("profile");
    let node_records = sandbox.dir("node");
    let git_env: Env = IDENTITY.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect();
    git(&sandbox, &git_env, &repo, &["init", "-q", "-b", "main"]);
    git(&sandbox, &git_env, &repo, &["add", "-A"]);
    git(&sandbox, &git_env, &repo, &["commit", "-q", "-m", "synthetic launcher commit"]);
    let commit = git(&sandbox, &git_env, &repo, &["rev-parse", "HEAD"]);
    // The launcher's checkout names the GitHub repository that a live checkout names, and a
    // `url.<bare>.insteadOf` rule sends every local fetch and push of it to a bare copy of the repository, so no test
    // reaches GitHub. `git remote get-url` reports that local copy, which the launcher sends the runner to clone; the
    // in-flight list reads the URL as configured, so it names the GitHub repository. The direction clone names another
    // origin, so a test can tell which one the runner cloned from.
    let runtime_origin = sandbox.dir("remotes/runtime.git");
    let runtime_origin_text = runtime_origin.display().to_string();
    git(&sandbox, &git_env, &repo, &["clone", "-q", "--bare", ".", &runtime_origin_text]);
    git(&sandbox, &git_env, &repo, &["config", &format!("url.{runtime_origin_text}.insteadOf"), GITHUB_ORIGIN]);
    git(&sandbox, &git_env, &repo, &["remote", "add", "origin", GITHUB_ORIGIN]);
    git(&sandbox, &git_env, &repo, &["fetch", "-q", "origin"]);

    sandbox.stub("gh", GH);
    sandbox.stub("ssh", SSH);
    // The local claude that a cron session's own turn runs.
    sandbox.stub("claude", CLAUDE);

    let instructions = sandbox.root().join("harness_instructions.md");
    fs::write(&instructions, INSTRUCTIONS).expect("the instructions file");

    let session_dir = sandbox.dir(&format!("profile/sessions/{SESSION_ID}"));
    fs::write(
      session_dir.join("metadata.json"),
      format!("{{\n  \"id\": \"{SESSION_ID}\",\n  \"task_parent_id\": \"{PARENT_ID}\"\n}}\n"),
    )
    .expect("the session metadata");
    let cron_dir = sandbox.dir("profile/config.d/cron.d");
    fs::write(
      cron_dir.join(format!("improve-{DIRECTION}.yaml")),
      format!(
        "cron: 31 * * * *\nbackend: synthetic\nprompt_file: dev/improve/{DIRECTION}.md\nsession_id: {PARENT_ID}\n"
      ),
    )
    .expect("the cron task file");
    fs::write(profile_home.join("skills_leak_patterns.local.txt"), "# local patterns\nfictionalmarker\n")
      .expect("the leak pattern file");

    let round_home = sandbox.dir("remote/improve/home");
    fs::write(
      round_home.join("config.yaml"),
      "backends:\n  options:\n    - id: synthetic-review\n      type: cc-anthropic-compatible\n      model: synthetic-model\n      api_base: https://gw.example\n      context_window: 4096\n      credential: synthetic-gateway\n",
    )
    .expect("the round's config.yaml");
    let round_root = sandbox.dir(&format!("remote/improve/{DIRECTION}"));
    let clone = round_root.join("charlie-bot");
    fs::create_dir_all(&clone).expect("the direction clone");
    git(&sandbox, &git_env, &clone, &["init", "-q", "-b", "main"]);
    git(&sandbox, &git_env, &clone, &["config", "core.hooksPath", "dev/git-hooks"]);
    git(&sandbox, &git_env, &clone, &["remote", "add", "origin", "https://example.invalid/someone/charlie-bot.git"]);
    let evidence = round_root.join("evidence");
    fs::create_dir_all(&evidence).expect("the evidence directory");
    let mock_home_log = sandbox.root().join("mock-home.log");
    let mock_home_fixture = sandbox.root().join("mock-home-fixture.sh");
    write_executable(&mock_home_fixture, format!("#!/usr/bin/env bash\n{MOCK_HOME}"));

    let remote_bin = sandbox.dir("remote/.local/bin");
    for (name, body) in [
      ("claude", CLAUDE),
      ("charlie-code", "exit 0\n"),
      ("systemd-run", SYSTEMD_RUN),
      ("systemctl", SYSTEMCTL),
      ("cargo", CARGO),
    ] {
      write_executable(&remote_bin.join(name), format!("#!/usr/bin/env bash\n{body}"));
    }
    // The round tools of this commit are already built, so the runner skips its build for them.
    let runner_bin = sandbox.dir("remote/improve/runner/bin");
    for (name, body) in [("charliebot", "exit 0\n"), ("improve-sandbox", SANDBOX), ("mock-home", MOCK_HOME)] {
      write_executable(&runner_bin.join(format!("{name}-{commit}")), format!("#!/usr/bin/env bash\n{body}"));
    }

    Self {
      ssh_args: sandbox.root().join("ssh-arguments.log"),
      claude_argv: evidence.join("claude-argv.txt"),
      claude_env: evidence.join("claude-env.txt"),
      claude_inflight: evidence.join("claude-inflight.txt"),
      claude_stdin: evidence.join("claude-stdin.txt"),
      systemctl_log: sandbox.root().join("systemctl.log"),
      systemd_run_log: sandbox.root().join("systemd-run.log"),
      scope_stopped: sandbox.root().join("scope-stopped"),
      mock_home_log,
      mock_home_fixture,
      node_claude_argv: node_records.join("claude-argv.txt"),
      node_claude_env: node_records.join("claude-env.txt"),
      node_claude_stdin: node_records.join("claude-stdin.txt"),
      instructions,
      commit,
      sandbox,
      repo,
      local_home,
      remote_home,
      profile_home,
      local_config_dir: LOCAL_CONFIG_DIR,
    }
  }

  pub fn launcher(&self) -> PathBuf {
    self.repo.join("dev/improve/remote-launch.sh")
  }

  /// The variables the launcher sees: a clean environment, like the server's child environment.
  pub fn env(&self) -> Env {
    let mut env = Env::new();
    env.insert("PATH".to_string(), format!("{}:/usr/local/bin:/usr/bin:/bin", self.sandbox.bin().display()));
    env.insert("HOME".to_string(), self.local_home.display().to_string());
    env.insert("TMPDIR".to_string(), self.sandbox.dir("local-tmp").display().to_string());
    env.insert("CHARLIEBOT_HOME".to_string(), self.profile_home.display().to_string());
    env.insert("CHARLIEBOT_SESSION_ID".to_string(), SESSION_ID.to_string());
    env.insert("CHARLIEBOT_RUN_TOKEN".to_string(), RUN_TOKEN.to_string());
    env.insert("IMPROVE_REMOTE_HOST".to_string(), REMOTE_HOST.to_string());
    env.insert("IMPROVE_EVIDENCE_COPY_SECONDS".to_string(), "30".to_string());
    env.insert("ANTHROPIC_BASE_URL".to_string(), "https://gateway.example".to_string());
    env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), GATEWAY_TOKEN.to_string());
    env.insert("ANTHROPIC_API_KEY".to_string(), API_KEY.to_string());
    env.insert("ANTHROPIC_MODEL".to_string(), "synthetic-model".to_string());
    env.insert("ANTHROPIC_CUSTOM_HEADERS".to_string(), "x-tag: synthetic\nx-line: two".to_string());
    env.insert("CLAUDE_CODE_EXTRA_BODY".to_string(), "{\"cache\":true}".to_string());
    env.insert("CLAUDE_CONFIG_DIR".to_string(), self.local_config_dir.to_string());
    env.insert("BASH_DEFAULT_TIMEOUT_MS".to_string(), "900000".to_string());
    env.insert("BASH_MAX_TIMEOUT_MS".to_string(), "900000".to_string());
    env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
    env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
    // The fake ssh's own settings, which a real ssh does not send.
    env.insert("SSH_ARG_LOG".to_string(), self.ssh_args.display().to_string());
    env.insert("SSH_REMOTE_HOME".to_string(), self.remote_home.display().to_string());
    env.insert(
      "SSH_REMOTE_PATH".to_string(),
      format!("{}:/usr/local/bin:/usr/bin:/bin", self.remote_home.join(".local/bin").display()),
    );
    env.insert("FAKE_CLAUDE_ARGV".to_string(), self.claude_argv.display().to_string());
    env.insert("FAKE_CLAUDE_ENV".to_string(), self.claude_env.display().to_string());
    env.insert("FAKE_CLAUDE_INFLIGHT".to_string(), self.claude_inflight.display().to_string());
    env.insert("FAKE_SYSTEMCTL_LOG".to_string(), self.systemctl_log.display().to_string());
    env.insert("FAKE_SYSTEMD_RUN_LOG".to_string(), self.systemd_run_log.display().to_string());
    env.insert("FAKE_SCOPE_STOPPED".to_string(), self.scope_stopped.display().to_string());
    env.insert("FAKE_MOCK_HOME_LOG".to_string(), self.mock_home_log.display().to_string());
    env.insert("FAKE_MOCK_HOME_FIXTURE".to_string(), self.mock_home_fixture.display().to_string());
    env.insert(
      "FAKE_MOCK_HOME_DIR".to_string(),
      self.remote_home.join("improve/mock-home").join(MOCK_HOME_VERSION).display().to_string(),
    );
    env
  }

  /// The variables of a cron session's own turn: the world's environment with the cron session's id,
  /// which a cron task file binds, and the local claude's records.
  pub fn node_env(&self) -> Env {
    let mut env = self.env();
    env.insert("CHARLIEBOT_SESSION_ID".to_string(), PARENT_ID.to_string());
    env.insert("FAKE_CLAUDE_ARGV".to_string(), self.node_claude_argv.display().to_string());
    env.insert("FAKE_CLAUDE_ENV".to_string(), self.node_claude_env.display().to_string());
    env
  }

  /// The command line the server builds for a worker run of this backend.
  pub fn claude_arguments(&self) -> Vec<String> {
    vec![
      "-p".to_string(),
      "--output-format".to_string(),
      "stream-json".to_string(),
      "--settings".to_string(),
      "{\"disableClaudeAiConnectors\":true}".to_string(),
      "--append-system-prompt-file".to_string(),
      self.instructions.display().to_string(),
      "--mcp-config".to_string(),
      "{\"mcpServers\":{\"charliebot-web\":{\"url\":\"http://localhost:18499/api/web-search/mcp\"}}}".to_string(),
    ]
  }

  /// One launcher command with the world's environment and the round's arguments.
  pub fn launcher_command(&self, env: &Env) -> std::process::Command {
    let mut command = harness::command("bash", env);
    command.arg(self.launcher()).args(self.claude_arguments());
    command
  }

  pub fn run(&self, env: &Env) -> harness::Run {
    harness::run(&mut self.launcher_command(env))
  }

  /// Whether the fake claude ran at all.
  pub fn claude_ran(&self) -> bool {
    self.claude_argv.exists()
  }

  pub fn claude_arguments_received(&self) -> Vec<String> {
    fs::read_to_string(&self.claude_argv)
      .expect("the fake claude recorded its arguments")
      .lines()
      .map(str::to_string)
      .collect()
  }

  pub fn claude_environment(&self) -> Env {
    let text = fs::read_to_string(&self.claude_env).expect("the fake claude recorded its environment");
    text
      .lines()
      .filter_map(|line| line.split_once('='))
      .map(|(name, value)| (name.to_string(), value.to_string()))
      .collect()
  }

  pub fn ssh_arguments(&self) -> String {
    fs::read_to_string(&self.ssh_args).unwrap_or_default()
  }

  /// Whether a cron session's own turn ran the local claude.
  pub fn node_claude_ran(&self) -> bool {
    self.node_claude_argv.exists()
  }

  pub fn node_claude_arguments(&self) -> Vec<String> {
    fs::read_to_string(&self.node_claude_argv)
      .expect("the local claude recorded its arguments")
      .lines()
      .map(str::to_string)
      .collect()
  }

  pub fn node_claude_environment(&self) -> Env {
    fs::read_to_string(&self.node_claude_env)
      .expect("the local claude recorded its environment")
      .lines()
      .filter_map(|line| line.split_once('='))
      .map(|(name, value)| (name.to_string(), value.to_string()))
      .collect()
  }

  pub fn systemctl_calls(&self) -> String {
    fs::read_to_string(&self.systemctl_log).unwrap_or_default()
  }

  /// The launch directory, when the round left one behind.
  pub fn launch_directory(&self) -> Option<PathBuf> {
    let root = self.remote_home.join("improve/launch");
    fs::read_dir(root).ok()?.flatten().map(|entry| entry.path()).next()
  }

  /// The evidence directory that the launcher copies back to.
  pub fn local_evidence(&self) -> PathBuf {
    self.local_home.join("improve").join(DIRECTION).join("evidence")
  }

  /// Every path under `root`, relative to it.
  pub fn tree(&self, root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    collect(root, root, &mut found);
    found.sort();
    found
  }

  /// A standalone clone of another direction at `improve/<name>/charlie-bot`, cloned from its own bare origin at
  /// `remotes/<name>.git`, as the host worker's five clones are on the remote host.
  pub fn other_direction_clone(&self, name: &str) -> PathBuf {
    let origin = self.sandbox.dir(&format!("remotes/{name}.git"));
    let origin_text = origin.display().to_string();
    let seed = self.sandbox.dir(&format!("seeds/{name}"));
    self.git(&seed, &["init", "-q", "-b", "main"]);
    fs::write(seed.join("base.md"), "base\n").expect("the seed's base file");
    self.git(&seed, &["add", "-A"]);
    self.git(&seed, &["commit", "-q", "-m", "synthetic base"]);
    self.git(&seed, &["clone", "-q", "--bare", ".", &origin_text]);
    let root = self.sandbox.dir(&format!("remote/improve/{name}"));
    self.git(&root, &["clone", "-q", &origin_text, "charlie-bot"]);
    root.join("charlie-bot")
  }

  /// The bare origin of an other direction's clone.
  pub fn other_direction_origin(&self, name: &str) -> PathBuf {
    self.sandbox.root().join("remotes").join(format!("{name}.git"))
  }

  /// A git command in `cwd` with the fixture identity, for building remote clones.
  pub fn git(&self, cwd: &Path, args: &[&str]) -> String {
    let extra: Env = IDENTITY.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect();
    git(&self.sandbox, &extra, cwd, args)
  }
}

impl Default for World {
  fn default() -> Self {
    Self::new()
  }
}

fn collect(root: &Path, dir: &Path, found: &mut Vec<String>) {
  let Ok(entries) = fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    if let Ok(relative) = path.strip_prefix(root) {
      found.push(relative.display().to_string());
    }
    if path.is_dir() {
      collect(root, &path, found);
    }
  }
}

fn git(sandbox: &Sandbox, extra: &Env, cwd: &Path, args: &[&str]) -> String {
  let mut env = sandbox.env();
  for (name, value) in extra {
    env.insert(name.clone(), value.clone());
  }
  env.insert("HOME".to_string(), sandbox.root().display().to_string());
  env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
  env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
  let mut command = harness::command("git", &env);
  command.arg("-C").arg(cwd).args(args);
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "git {args:?} in {}: {}", cwd.display(), run.describe());
  run.stdout.trim().to_string()
}
