//! The improve round's test world: a throwaway repository that holds the launcher, a profile home, a remote home and
//! the stub commands of both sides.
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

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD as URL_SAFE_NO_PAD;
use charliebot_shell_harness::{self as harness, Env, Sandbox, write_executable};
use sha2::{Digest as _, Sha256};

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
/// The gateway token of a gateway member: the server's `ANTHROPIC_AUTH_TOKEN`.
pub const GATEWAY_TOKEN: &str = "synthetic-gateway-token";
/// The review option's key, which the profile home's credentials file holds under the review credential.
pub const REVIEW_KEY: &str = "synthetic-review-key";
/// The review option's credential name, as `config.yaml` and `credentials.yaml` name it.
pub const REVIEW_CREDENTIAL: &str = "aigw_charliebot_agents";
/// The review option's model, API base and context window, as the profile home's `config.yaml` holds them.
pub const REVIEW_MODEL: &str = "kimi-k3.1-pro";
pub const REVIEW_API_BASE: &str = "https://aigw.example";
pub const REVIEW_CONTEXT_WINDOW: &str = "409600";
/// The access token of a subscription member: the `accessToken` of its login file.
pub const LOGIN_TOKEN: &str = "synthetic-subscription-access-token";
/// The refresh token of that login. It stays on the launching host, so no round may receive it.
pub const LOGIN_REFRESH_TOKEN: &str = "synthetic-subscription-refresh-token";
/// The launcher host's own Claude login, which must not be forwarded to the improve host.
pub const LOCAL_CLAUDE_OAUTH_TOKEN: &str = "synthetic-local-claude-oauth-token";
/// The refresh token of the host's Codex login. It stays on the launching host.
pub const CODEX_REFRESH_TOKEN: &str = "synthetic-codex-refresh-token";
/// The ChatGPT id token of the host's Codex login.
pub const CODEX_ID_TOKEN: &str = "synthetic-codex-id-token";
/// The account id of the host's Codex login.
pub const CODEX_ACCOUNT_ID: &str = "synthetic-codex-account";
/// The `last_refresh` of the host's Codex login, which the forwarded login keeps.
pub const CODEX_LAST_REFRESH: &str = "2026-11-14T17:05:27Z";
/// The least access-token life a round may start with, in seconds: the server's `IMPROVE_TOKEN_MIN_SECONDS`.
pub const TOKEN_MIN_SECONDS: &str = "14400";
pub const RUN_TOKEN: &str = "synthetic-run-token";
pub const REMOTE_HOST: &str = "improve.example.ts.net";
pub const MOCK_HOME_VERSION: &str = "2";
pub const LOCAL_CONFIG_DIR: &str = "/synthetic/local-claude-config";
pub const INSTRUCTIONS: &str = "Improve one topic.\n";
/// The gateway variables that a subscription member's launch environment does not carry.
const GATEWAY_VARIABLES: [&str; 5] = [
  "ANTHROPIC_BASE_URL",
  "ANTHROPIC_AUTH_TOKEN",
  "ANTHROPIC_MODEL",
  "ANTHROPIC_CUSTOM_HEADERS",
  "CLAUDE_CODE_EXTRA_BODY",
];

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
if [[ ${FAKE_SSH_PAUSE_MAIN_RUN:-0} == 1 && $remote == *'remote-runner.sh '* && $remote != *'--prepare '* ]]; then
  : >"$FAKE_SSH_MAIN_RUN_READY"
  while [[ ! -e $FAKE_SSH_MAIN_RUN_RELEASE ]]; do
    sleep 0.01
  done
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
printf '%s\n' "$0" >>"$FAKE_CLI_RUNS"
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
# The record keeps every key out of the evidence: a real claude holds its environment in memory only, so the record
# carries each key's digest in its place, and `unset` for a key that the environment does not hold.
env | grep -v -e '^ANTHROPIC_AUTH_TOKEN=' -e '^CHARLIE_CODE_API_KEY=' -e '^CLAUDE_CODE_OAUTH_TOKEN=' >"$FAKE_CLAUDE_ENV"
for name in ANTHROPIC_AUTH_TOKEN CHARLIE_CODE_API_KEY CLAUDE_CODE_OAUTH_TOKEN; do
  if [[ -n ${!name+x} ]]; then
    printf '%s_SHA256=%s\n' "$name" "$(printf '%s' "${!name}" | sha256sum | cut -d' ' -f1)"
  else
    printf '%s_SHA256=unset\n' "$name"
  fi
done >>"$FAKE_CLAUDE_ENV"
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
# The round's review: the command the round's agent runs, with no model and no API base flag.
if [[ -n ${FAKE_CLAUDE_REVIEW_TASK:-} ]]; then
  charlie-code --json --task-file "$FAKE_CLAUDE_REVIEW_TASK" \
    --session-dir "$HOME/improve/$IMPROVE_DIRECTION/clc-sessions"
fi
exit "${FAKE_CLAUDE_EXIT:-0}"
"#;

/// Fake codex: record the `CODEX_HOME` it runs with, its arguments, and the `auth.json` it finds there with that file's
/// mode, outside the round's boundary.
const CODEX: &str = r#"# Fake codex: record the CODEX_HOME it runs with, the arguments it receives and its login file.
printf '%s\n' "$0" >>"$FAKE_CLI_RUNS"
printf 'CODEX_HOME=%s\n' "${CODEX_HOME-unset}" >"$FAKE_CODEX_ENV"
printf '%s\n' "$@" >"$FAKE_CODEX_ARGV"
if [[ -n ${CODEX_HOME-} && -f $CODEX_HOME/auth.json ]]; then
  cp -- "$CODEX_HOME/auth.json" "$FAKE_CODEX_AUTH"
  stat -c %a -- "$CODEX_HOME/auth.json" >"$FAKE_CODEX_AUTH_MODE"
fi
exit 0
"#;

/// The version lines of this host's CLIs, which the launcher reads with `--version`.
pub const LOCAL_CLAUDE_VERSION: &str = "2.1.328";
pub const LOCAL_CLAUDE_LINE: &str = "2.1.328 (Claude Code)";
pub const LOCAL_CODEX_VERSION: &str = "0.167.0";
pub const LOCAL_CODEX_LINE: &str = "codex-cli 0.167.0";
/// The stale copies in the host's own `~/.local/bin`, older than the versions the launcher reads. A round must never run
/// them.
const HOST_CLAUDE_LINE: &str = "2.1.260 (Claude Code)";
const HOST_CODEX_LINE: &str = "codex-cli 0.145.0";

/// The launcher's view of this host's claude: it reads the `--version` line and runs nothing else.
const LOCAL_CLAUDE: &str = r#"# Fake local claude: the launcher reads its --version line, and no round runs it on this host.
if [[ ${1-} == --version ]]; then
  printf '%s\n' "$FAKE_LOCAL_CLAUDE_LINE"
  exit 0
fi
printf 'the launcher ran the local claude: %s\n' "$*" >&2
exit 1
"#;

/// The launcher's view of this host's codex: it reads the `--version` line and runs nothing else.
const LOCAL_CODEX: &str = r#"# Fake local codex: the launcher reads its --version line, and no round runs it on this host.
if [[ ${1-} == --version ]]; then
  printf '%s\n' "$FAKE_LOCAL_CODEX_LINE"
  exit 0
fi
printf 'the launcher ran the local codex: %s\n' "$*" >&2
exit 1
"#;

/// Fake curl: serves a URL from the fake vendor root as `curl -f` does, so a URL the vendor does not publish fails with
/// 22. Every URL is recorded, and FAKE_CURL_SLEEP holds each call, so two installs can overlap.
const CURL: &str = r#"# Fake curl: serve the URL from the fake vendor root, as curl -f does.
url=${@: -1}
output=
previous=
for argument in "$@"; do
  if [[ $previous == -o ]]; then
    output=$argument
  fi
  previous=$argument
done
printf '%s\n' "$url" >>"$FAKE_CURL_LOG"
sleep "${FAKE_CURL_SLEEP:-0}"
source_path="$FAKE_VENDOR_ROOT/${url#https://}"
if [[ $url != https://* || ! -f $source_path ]]; then
  printf 'curl: (22) The requested URL returned error: 404\n' >&2
  exit 22
fi
if [[ -n $output ]]; then
  cp -- "$source_path" "$output"
else
  cat -- "$source_path"
fi
"#;

/// A member CLI as the vendor publishes it: `--version` prints `line`, and any other call runs `body`.
fn cli_script(body: &str, line: &str) -> String {
  format!("#!/usr/bin/env bash\nif [[ ${{1-}} == --version ]]; then\n  printf '%s\\n' '{line}'\n  exit 0\nfi\n{body}")
}

/// The SHA-256 of a file, in the form the vendor publishes.
fn sha256_file(path: &Path) -> String {
  let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
  format!("{:x}", Sha256::digest(bytes))
}

/// Fake charlie-code: record the arguments and environment it receives. The key goes in as its digest.
const CHARLIE_CODE: &str = r#"# Fake charlie-code: record the arguments and environment it receives.
printf '%s\n' "$@" >"$FAKE_CHARLIE_CODE_ARGV"
env | grep -v -e '^CHARLIE_CODE_API_KEY=' >"$FAKE_CHARLIE_CODE_ENV"
if [[ -n ${CHARLIE_CODE_API_KEY+x} ]]; then
  printf 'CHARLIE_CODE_API_KEY_SHA256=%s\n' "$(printf '%s' "$CHARLIE_CODE_API_KEY" | sha256sum | cut -d' ' -f1)" \
    >>"$FAKE_CHARLIE_CODE_ENV"
else
  printf 'CHARLIE_CODE_API_KEY_SHA256=unset\n' >>"$FAKE_CHARLIE_CODE_ENV"
fi
exit 0
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

/// Fake improve-sandbox: no boundary. It runs the command after `--`, as the real program does. With
/// `FAKE_SANDBOX_REFUSE=1` it refuses to start, as a kernel without Landlock does.
const SANDBOX: &str = r#"# Fake improve-sandbox: no boundary. It runs the command after `--`, as the real program does.
if [[ ${FAKE_SANDBOX_REFUSE:-0} == 1 ]]; then
  printf 'improve-sandbox: Landlock is not enforced by this kernel\n' >&2
  exit 1
fi
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

/// The current time in whole seconds since the epoch.
pub fn now_seconds() -> i64 {
  std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("the clock is after the epoch").as_secs()
    as i64
}

/// A synthetic ChatGPT access token whose payload carries `exp`. The launcher reads only that claim.
pub fn codex_access_token(exp: i64) -> String {
  let encode = |text: String| URL_SAFE_NO_PAD.encode(text);
  format!("{}.{}.synthetic-signature", encode(r#"{"alg":"none"}"#.to_string()), encode(format!(r#"{{"exp":{exp}}}"#)))
}

/// The host's `auth.json` for a codex login whose access token expires at `exp`. It holds a refresh token, which the
/// launch must not forward.
pub fn codex_login_json(exp: i64) -> String {
  serde_json::json!({
    "auth_mode": "chatgpt",
    "last_refresh": CODEX_LAST_REFRESH,
    "tokens": {
      "id_token": CODEX_ID_TOKEN,
      "access_token": codex_access_token(exp),
      "refresh_token": CODEX_REFRESH_TOKEN,
      "account_id": CODEX_ACCOUNT_ID,
    },
  })
  .to_string()
}

/// The file every world locks for its lifetime. Each round of a direction writes under `/tmp/improve-<direction>`, and
/// its start removes an idle direction's Codex login from there, so two worlds never run together: one test's round
/// would otherwise remove the login another test's codex round is reading. Nextest runs each test in its own process, so
/// the lock is a file lock that every process shares, not an in-process mutex.
const TMP_ROOT_LOCK_PATH: &str = "/tmp/improve-tests.lock";

/// Holds the shared tmp-root lock until the world is dropped.
fn lock_tmp_root() -> File {
  let file = File::create(TMP_ROOT_LOCK_PATH).expect("the improve tests' tmp-root lock file");
  file.lock().expect("the improve tests' tmp-root lock");
  file
}

/// One test's repository, both homes and stub directory.
pub struct World {
  _tmp_root: File,
  direction: &'static str,
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
  /// Where the fake charlie-code writes its arguments and environment, inside the round's boundary.
  pub charlie_code_argv: PathBuf,
  pub charlie_code_env: PathBuf,
  /// Where the fake codex writes its `CODEX_HOME` and its arguments: this host's records, outside the round's boundary.
  pub codex_env: PathBuf,
  pub codex_argv: PathBuf,
  /// Where the fake codex copies the `auth.json` it finds under its CODEX_HOME, and that file's mode.
  pub codex_auth: PathBuf,
  pub codex_auth_mode: PathBuf,
  /// The login directory of a subscription member: its `CLAUDE_CONFIG_DIR`, which holds `.credentials.json`.
  pub claude_login_dir: PathBuf,
  pub systemctl_log: PathBuf,
  pub systemd_run_log: PathBuf,
  pub scope_stopped: PathBuf,
  pub mock_home_log: PathBuf,
  pub mock_home_fixture: PathBuf,
  /// The fake vendor's root: each URL the fake curl serves is a file under its host and path.
  pub vendor_root: PathBuf,
  /// Where the fake curl records each URL it serves.
  pub curl_log: PathBuf,
  /// Where each fake member CLI records its own path when it runs, so a test sees which copy a round ran.
  pub cli_runs: PathBuf,
  /// The in-memory `$CLAUDE_CONFIG_DIR` of the launcher. It must not reach the round.
  pub local_config_dir: &'static str,
}

impl World {
  pub fn new() -> Self {
    Self::new_with_direction(DIRECTION)
  }

  pub fn new_with_direction(direction: &'static str) -> Self {
    let tmp_root = lock_tmp_root();
    let sandbox = Sandbox::new();
    let repo = sandbox.dir("repo");
    let scripts = sandbox.dir("repo/dev/improve");
    for name in ["remote-launch.sh", "remote-runner.sh", "inflight-files.sh", "member_kinds.txt"] {
      harness::copy_file(&harness::repo_root().join("dev/improve").join(name), &scripts.join(name));
    }
    let local_home = sandbox.dir("local");
    let remote_home = sandbox.dir("remote");
    let profile_home = sandbox.dir("profile");
    let git_env: Env = IDENTITY.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect();
    git(&sandbox, &git_env, &repo, &["init", "-q", "-b", "main"]);
    git(&sandbox, &git_env, &repo, &["add", "-A"]);
    git(&sandbox, &git_env, &repo, &["commit", "-q", "-m", "synthetic launcher commit"]);
    let commit = git(&sandbox, &git_env, &repo, &["rev-parse", "HEAD"]);
    // The launcher's checkout names the GitHub repository that a live checkout names. A
    // `url.<bare>.insteadOf` rule sends every local fetch and push of it to a bare copy of the repository, so no test
    // reaches GitHub. `git remote get-url` reports that local copy, which the launcher sends the runner to clone. The
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
      cron_dir.join(format!("improve-{direction}.yaml")),
      format!(
        "cron: 31 * * * *\nbackend: synthetic\nprompt_file: dev/improve/{direction}.md\nsession_id: {PARENT_ID}\n"
      ),
    )
    .expect("the cron task file");
    fs::write(profile_home.join("skills_leak_patterns.local.txt"), "# local patterns\nfictionalmarker\n")
      .expect("the leak pattern file");
    // The review option as this host's config.yaml lists it: a list item whose fields sit four spaces in.
    fs::write(
      profile_home.join("config.yaml"),
      format!(
        "backends:\n  options:\n  - api_base: {REVIEW_API_BASE}\n    context_window: {REVIEW_CONTEXT_WINDOW}\n    \
         credential: {REVIEW_CREDENTIAL}\n    id: claude-code-kimi-k3.1-pro\n    model: {REVIEW_MODEL}\n    \
         type: cc-anthropic-compatible\n  - effort: high\n    id: claude-fable-5\n    model: claude-fable-5-1\n"
      ),
    )
    .expect("the profile home's config.yaml");
    // A section before the review credential, so a test that reads the first api_key line reads the wrong key.
    fs::write(
      profile_home.join("credentials.yaml"),
      format!("synthetic-decoy:\n  api_key: synthetic-decoy-key\n{REVIEW_CREDENTIAL}:\n  api_key: {REVIEW_KEY}\n"),
    )
    .expect("the profile home's credentials.yaml");

    let round_root = sandbox.dir(&format!("remote/improve/{direction}"));
    sandbox.dir("remote/improve/home");
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

    // The launcher's CLIs report this host's versions. The improve host's own copies in `~/.local/bin` are older: a
    // round that ran either would show it in `cli_runs`.
    sandbox.stub("claude", LOCAL_CLAUDE);
    sandbox.stub("codex", LOCAL_CODEX);
    let remote_bin = sandbox.dir("remote/.local/bin");
    write_executable(&remote_bin.join("claude"), cli_script(CLAUDE, HOST_CLAUDE_LINE));
    write_executable(&remote_bin.join("codex"), cli_script(CODEX, HOST_CODEX_LINE));
    let vendor_root = sandbox.dir("vendor");
    let curl_log = sandbox.root().join("curl.log");
    let cli_runs = sandbox.root().join("cli-runs.log");
    for (name, body) in [
      ("curl", CURL),
      ("charlie-code", CHARLIE_CODE),
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

    let world = Self {
      _tmp_root: tmp_root,
      direction,
      ssh_args: sandbox.root().join("ssh-arguments.log"),
      claude_argv: evidence.join("claude-argv.txt"),
      claude_env: evidence.join("claude-env.txt"),
      claude_inflight: evidence.join("claude-inflight.txt"),
      claude_stdin: evidence.join("claude-stdin.txt"),
      charlie_code_argv: evidence.join("charlie-code-argv.txt"),
      charlie_code_env: evidence.join("charlie-code-env.txt"),
      codex_env: sandbox.root().join("codex-env.txt"),
      codex_argv: sandbox.root().join("codex-argv.txt"),
      codex_auth: sandbox.root().join("codex-auth.json"),
      codex_auth_mode: sandbox.root().join("codex-auth-mode.txt"),
      claude_login_dir: sandbox.dir("login/claude"),
      systemctl_log: sandbox.root().join("systemctl.log"),
      systemd_run_log: sandbox.root().join("systemd-run.log"),
      scope_stopped: sandbox.root().join("scope-stopped"),
      mock_home_log,
      mock_home_fixture,
      instructions,
      commit,
      sandbox,
      repo,
      local_home,
      remote_home,
      profile_home,
      local_config_dir: LOCAL_CONFIG_DIR,
      vendor_root,
      curl_log,
      cli_runs,
    };
    // The vendor publishes the defaults that the launcher's CLIs report, so a round installs them as the host would.
    world.publish_member_cli("claude", LOCAL_CLAUDE_VERSION, LOCAL_CLAUDE_LINE);
    world.publish_member_cli("codex", LOCAL_CODEX_VERSION, LOCAL_CODEX_LINE);
    world
  }

  /// Publish one member CLI version on the fake vendor, as the vendor publishes it. Codex: the package asset of its
  /// release, with the digest that GitHub lists for the asset. Claude: the native binary of its release, with the
  /// checksum that its manifest lists. The published CLI prints `reported` for `--version`.
  pub fn publish_member_cli(&self, kind: &str, version: &str, reported: &str) {
    let arch = std::env::consts::ARCH;
    match kind {
      "codex" => {
        // The package with its vendor layout: the executable beside the two helpers that tool calls start, and the
        // path and resource directories and the manifest that the install unpacks with them.
        let stage = self.sandbox.dir(&format!("stage/codex-{version}"));
        let bin = stage.join("bin");
        fs::create_dir_all(&bin).expect("the package's bin directory");
        write_executable(&bin.join("codex"), cli_script(CODEX, reported));
        for helper in ["codex-code-mode-host", "codex-exec-server"] {
          write_executable(&bin.join(helper), "#!/usr/bin/env bash\nexit 0\n");
        }
        fs::create_dir_all(stage.join("codex-path")).expect("the package's path directory");
        write_executable(&stage.join("codex-path/rg"), "#!/usr/bin/env bash\nexit 0\n");
        fs::create_dir_all(stage.join("codex-resources")).expect("the package's resource directory");
        write_executable(&stage.join("codex-resources/bwrap"), "#!/usr/bin/env bash\nexit 0\n");
        fs::write(stage.join("codex-package.json"), "{\"version\":\"synthetic\"}\n").expect("the package manifest");
        let asset = format!("codex-package-{arch}-unknown-linux-musl.tar.gz");
        let package =
          self.vendor_root.join(format!("github.com/openai/codex/releases/download/rust-v{version}/{asset}"));
        fs::create_dir_all(package.parent().expect("a package directory")).expect("the package directory");
        let status =
          Command::new("tar").arg("-czf").arg(&package).arg("-C").arg(&stage).arg(".").status().expect("tar runs");
        assert!(status.success(), "tar could not write the fake package {}", package.display());
        let release = self.vendor_root.join(format!("api.github.com/repos/openai/codex/releases/tags/rust-v{version}"));
        fs::create_dir_all(release.parent().expect("a release directory")).expect("the release directory");
        let digest = format!("sha256:{}", sha256_file(&package));
        let listing = serde_json::json!({
          "tag_name": format!("rust-v{version}"),
          "assets": [{"name": asset, "digest": digest}],
        });
        fs::write(&release, listing.to_string()).expect("the fake release listing");
      }
      "claude" => {
        let platform = match arch {
          "aarch64" => "linux-arm64",
          "x86_64" => "linux-x64",
          other => panic!("the fake vendor has no Claude build for {other}"),
        };
        let script = cli_script(CLAUDE, reported);
        let base = self.vendor_root.join(format!(
          "storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/{version}"
        ));
        let binary = base.join(platform).join("claude");
        fs::create_dir_all(binary.parent().expect("a platform directory")).expect("the platform directory");
        fs::write(&binary, &script).expect("the fake Claude binary");
        let manifest = serde_json::json!({
          "version": version,
          "platforms": {(platform): {"binary": "claude", "checksum": sha256_file(&binary), "size": script.len()}},
        });
        fs::write(base.join("manifest.json"), manifest.to_string()).expect("the fake Claude manifest");
      }
      other => panic!("the fake vendor publishes no {other}"),
    }
  }

  /// The probe form of the runner under the remote home: `remote-runner.sh --member-cli <kind> <version>`, with the
  /// remote PATH, so `curl` and the other tools are the stubs and the host's own.
  pub fn member_cli_command(&self, kind: &str, version: &str) -> Command {
    let mut env = self.env();
    env.insert("HOME".to_string(), self.remote_home.display().to_string());
    env.insert(
      "PATH".to_string(),
      format!("{}:/usr/local/bin:/usr/bin:/bin", self.remote_home.join(".local/bin").display()),
    );
    let mut command = harness::command("bash", &env);
    command.arg(self.repo.join("dev/improve/remote-runner.sh")).args(["--member-cli", kind, version]);
    command
  }

  /// A stand-in for a live round's wrapper: a process whose command line runs remote-runner.sh with `arguments`. It
  /// stays alive until `stop` exists.
  pub fn spawn_live_runner(&self, stop: &Path, arguments: &[&str]) -> Child {
    let script = self.sandbox.dir("live").join("remote-runner.sh");
    write_executable(&script, "while [[ ! -e \"$1\" ]]; do\n  sleep 0.05\ndone\n");
    Command::new("bash")
      .arg(&script)
      .arg(stop)
      .args(arguments)
      .stdin(Stdio::null())
      .spawn()
      .expect("the live runner stand-in starts")
  }

  /// The paths of the fake member CLIs that ran, in the order they ran.
  pub fn cli_runs(&self) -> Vec<String> {
    lines_or_empty(&self.cli_runs)
  }

  /// The URLs that the runner's curl fetched from the fake vendor, in order.
  pub fn curl_calls(&self) -> Vec<String> {
    lines_or_empty(&self.curl_log)
  }

  pub fn launcher(&self) -> PathBuf {
    self.repo.join("dev/improve/remote-launch.sh")
  }

  /// The variables the launcher sees: a clean environment, like the server's child environment. It is a gateway
  /// claude's environment: the server set the gateway variables and the gateway token.
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
    env.insert("ANTHROPIC_MODEL".to_string(), "synthetic-model".to_string());
    env.insert("ANTHROPIC_CUSTOM_HEADERS".to_string(), "x-tag: synthetic\nx-line: two".to_string());
    env.insert("CLAUDE_CODE_EXTRA_BODY".to_string(), "{\"cache\":true}".to_string());
    env.insert("CLAUDE_CONFIG_DIR".to_string(), self.local_config_dir.to_string());
    env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), LOCAL_CLAUDE_OAUTH_TOKEN.to_string());
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
    env.insert("FAKE_CODEX_ENV".to_string(), self.codex_env.display().to_string());
    env.insert("FAKE_CODEX_ARGV".to_string(), self.codex_argv.display().to_string());
    env.insert("FAKE_CODEX_AUTH".to_string(), self.codex_auth.display().to_string());
    env.insert("FAKE_CODEX_AUTH_MODE".to_string(), self.codex_auth_mode.display().to_string());
    env.insert("IMPROVE_TOKEN_MIN_SECONDS".to_string(), TOKEN_MIN_SECONDS.to_string());
    env.insert("FAKE_CHARLIE_CODE_ARGV".to_string(), self.charlie_code_argv.display().to_string());
    env.insert("FAKE_CHARLIE_CODE_ENV".to_string(), self.charlie_code_env.display().to_string());
    env.insert("FAKE_SYSTEMCTL_LOG".to_string(), self.systemctl_log.display().to_string());
    env.insert("FAKE_SYSTEMD_RUN_LOG".to_string(), self.systemd_run_log.display().to_string());
    env.insert("FAKE_SCOPE_STOPPED".to_string(), self.scope_stopped.display().to_string());
    env.insert("FAKE_MOCK_HOME_LOG".to_string(), self.mock_home_log.display().to_string());
    env.insert("FAKE_MOCK_HOME_FIXTURE".to_string(), self.mock_home_fixture.display().to_string());
    env.insert("FAKE_VENDOR_ROOT".to_string(), self.vendor_root.display().to_string());
    env.insert("FAKE_CURL_LOG".to_string(), self.curl_log.display().to_string());
    env.insert("FAKE_CLI_RUNS".to_string(), self.cli_runs.display().to_string());
    env.insert("FAKE_LOCAL_CLAUDE_LINE".to_string(), LOCAL_CLAUDE_LINE.to_string());
    env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), LOCAL_CODEX_LINE.to_string());
    env.insert(
      "FAKE_MOCK_HOME_DIR".to_string(),
      self.remote_home.join("improve/mock-home").join(MOCK_HOME_VERSION).display().to_string(),
    );
    env
  }

  /// The environment of a subscription member: the server strips the gateway variables from it and points
  /// `CLAUDE_CONFIG_DIR` at the member's login directory.
  pub fn host_login_env(&self) -> Env {
    let mut env = self.env();
    for name in GATEWAY_VARIABLES {
      env.remove(name);
    }
    env.insert("CLAUDE_CONFIG_DIR".to_string(), self.claude_login_dir.display().to_string());
    env
  }

  /// The server's command line for a claude member: the binary, then the claude arguments.
  pub fn agent_arguments(&self) -> Vec<String> {
    let mut arguments = vec!["claude".to_string()];
    arguments.extend(
      [
        "-p",
        "--output-format",
        "stream-json",
        "--settings",
        "{\"disableClaudeAiConnectors\":true}",
        "--append-system-prompt-file",
      ]
      .map(str::to_string),
    );
    arguments.push(self.instructions.display().to_string());
    arguments.extend(
      ["--mcp-config", "{\"mcpServers\":{\"charliebot-web\":{\"url\":\"http://localhost:18499/api/web-search/mcp\"}}}"]
        .map(str::to_string),
    );
    arguments
  }

  /// The server's command line for a codex member: the binary, then the codex arguments with the instructions in
  /// `--config developer_instructions`, and no instructions file.
  pub fn codex_arguments(&self) -> Vec<String> {
    [
      "codex",
      "exec",
      "--json",
      "--skip-git-repo-check",
      "--config",
      "developer_instructions=Improve one topic.",
      "--",
      "Improve one topic in the performance direction.",
    ]
    .map(str::to_string)
    .to_vec()
  }

  /// One launcher command with the world's environment and the given command line.
  pub fn launcher_command_for(&self, env: &Env, command_line: &[String]) -> std::process::Command {
    let mut command = harness::command("bash", env);
    command.arg(self.launcher()).args(command_line);
    command
  }

  /// One launcher command for a claude member with the world's environment.
  pub fn launcher_command(&self, env: &Env) -> std::process::Command {
    self.launcher_command_for(env, &self.agent_arguments())
  }

  pub fn run(&self, env: &Env) -> harness::Run {
    harness::run(&mut self.launcher_command(env))
  }

  /// Whether the fake claude ran at all.
  pub fn claude_ran(&self) -> bool {
    self.claude_argv.exists()
  }

  pub fn claude_arguments_received(&self) -> Vec<String> {
    record_lines(&self.claude_argv)
  }

  pub fn claude_environment(&self) -> Env {
    record_environment(&self.claude_env)
  }

  /// Whether the fake charlie-code ran at all.
  pub fn charlie_code_ran(&self) -> bool {
    self.charlie_code_argv.exists()
  }

  pub fn charlie_code_arguments_received(&self) -> Vec<String> {
    record_lines(&self.charlie_code_argv)
  }

  pub fn charlie_code_environment(&self) -> Env {
    record_environment(&self.charlie_code_env)
  }

  /// Whether the fake codex ran at all.
  pub fn codex_ran(&self) -> bool {
    self.codex_env.exists()
  }

  /// The `CODEX_HOME` that the fake codex ran with, as its record holds it.
  pub fn codex_home_received(&self) -> String {
    fs::read_to_string(&self.codex_env)
      .expect("the fake codex recorded its environment")
      .lines()
      .find_map(|line| line.strip_prefix("CODEX_HOME="))
      .expect("the record names CODEX_HOME")
      .to_string()
  }

  pub fn codex_arguments_received(&self) -> Vec<String> {
    fs::read_to_string(&self.codex_argv)
      .expect("the fake codex recorded its arguments")
      .lines()
      .map(str::to_string)
      .collect()
  }

  pub fn ssh_arguments(&self) -> String {
    fs::read_to_string(&self.ssh_args).unwrap_or_default()
  }

  pub fn systemctl_calls(&self) -> String {
    fs::read_to_string(&self.systemctl_log).unwrap_or_default()
  }

  /// Whether the runner started a systemd scope.
  pub fn scope_started(&self) -> bool {
    self.systemd_run_log.exists()
  }

  /// The login file of a subscription member: `.credentials.json` in its login directory.
  pub fn claude_login_file(&self) -> PathBuf {
    self.claude_login_dir.join(".credentials.json")
  }

  /// Write the member's login file, whose access token expires at `expires_at_ms` (milliseconds since the epoch).
  pub fn write_claude_login(&self, expires_at_ms: i64) {
    fs::create_dir_all(&self.claude_login_dir).expect("the Claude login directory");
    let login = serde_json::json!({
      "claudeAiOauth": {
        "accessToken": LOGIN_TOKEN,
        "refreshToken": LOGIN_REFRESH_TOKEN,
        "expiresAt": expires_at_ms,
        "scopes": ["user:inference"],
      }
    });
    fs::write(self.claude_login_file(), login.to_string()).expect("the Claude login file");
  }

  /// The host's Codex login file, `<local home>/.codex/auth.json`, whose access token expires at `exp` (seconds).
  pub fn write_codex_login(&self, exp: i64) {
    let codex_home = self.local_home.join(".codex");
    fs::create_dir_all(&codex_home).expect("the Codex home");
    fs::write(codex_home.join("auth.json"), codex_login_json(exp)).expect("the Codex login");
  }

  /// The CODEX_HOME that a codex round of this world uses on the host: a directory under the direction's /tmp root.
  pub fn codex_home_directory(&self) -> PathBuf {
    PathBuf::from(format!("/tmp/improve-{}/codex-home", self.direction))
  }

  /// A launch directory the runner reads, created as the launcher's payload creates it: the member kind list
  /// that the payload carries sits beside the runner.
  pub fn stage_launch(&self, launch_id: &str) -> PathBuf {
    let launch = self.remote_home.join("improve/launch").join(launch_id);
    fs::create_dir_all(&launch).expect("the launch directory");
    harness::copy_file(&self.repo.join("dev/improve/member_kinds.txt"), &launch.join("member_kinds.txt"));
    launch
  }

  /// The launch directory, when the round left one behind.
  pub fn launch_directory(&self) -> Option<PathBuf> {
    let root = self.remote_home.join("improve/launch");
    fs::read_dir(root).ok()?.flatten().map(|entry| entry.path()).next()
  }

  /// The evidence directory that the launcher copies back to.
  pub fn local_evidence(&self) -> PathBuf {
    self.local_home.join("improve").join(self.direction).join("evidence")
  }

  /// Every path under `root`, relative to it.
  pub fn tree(&self, root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    collect(root, root, &mut found);
    found.sort();
    found
  }

  /// A standalone clone of another direction at `improve/<name>/charlie-bot`. It is cloned from its own bare
  /// origin at `remotes/<name>.git`, as the host worker's five clones are on the remote host.
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

/// The lines of a fake command's record of its arguments, one argument per line.
fn record_lines(path: &Path) -> Vec<String> {
  fs::read_to_string(path).expect("the fake command recorded its arguments").lines().map(str::to_string).collect()
}

/// The lines of a fake command's record, or none when the command has not run.
fn lines_or_empty(path: &Path) -> Vec<String> {
  fs::read_to_string(path).unwrap_or_default().lines().map(str::to_string).collect()
}

/// The `NAME=value` lines of a fake command's record of its environment.
fn record_environment(path: &Path) -> Env {
  fs::read_to_string(path)
    .expect("the fake command recorded its environment")
    .lines()
    .filter_map(|line| line.split_once('='))
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect()
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
