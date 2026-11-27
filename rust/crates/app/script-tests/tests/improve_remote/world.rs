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

/// The direction of every test round. Uses a synthetic name so host tests never touch real rounds' `/tmp/improve-*`.
pub const DIRECTION: &str = "test_perf";
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
/// The review backend's key, which the profile home's credentials file holds under the review credential.
pub const REVIEW_KEY: &str = "synthetic-review-key";
/// The review backend's credential name, as the resolve answer and `credentials.yaml` name it.
pub const REVIEW_CREDENTIAL: &str = "synthetic-review-credential";
/// The review option that `backends.calls.improve_review` resolves to: its id, model, API base and context window.
pub const SYNTHETIC_REVIEW_ID: &str = "synthetic-review";
pub const REVIEW_MODEL: &str = "synthetic-review-model";
pub const REVIEW_API_BASE: &str = "https://aigw.example";
pub const REVIEW_CONTEXT_WINDOW: u64 = 409_600;
/// The probe answer that `backends.calls.probe` resolves to, as the server prints it: the option's record with no key.
/// The launch ships it to the round, whose serve trial reads it there.
pub const SYNTHETIC_PROBE_ANSWER: &str = r#"{"option":{"id":"synthetic-probe","label":"Synthetic probe","type":"charlie-code","model":"synthetic-probe-model","api_base":"https://probe.example/v1"}}"#;
/// The review option types of the gateway key and of the subscription account, whose logins the launcher reads.
pub const GATEWAY_REVIEW_TYPE: &str = "cc-anthropic-compatible";
pub const SUBSCRIPTION_REVIEW_TYPE: &str = "cc-claude";
/// The access token of a subscription review's account: the `accessToken` of its login file.
pub const REVIEW_LOGIN_TOKEN: &str = "synthetic-review-access-token";
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
/// The pinned Playwright build and its Linux layout, which the round's start check looks for under the runner's home.
pub const BROWSER_BUILD: &str = "chromium-1248";
pub const BROWSER_LAYOUT: &str = "chrome-linux-arm64";
/// The synthetic Chrome that a round's start check runs: it prints a page and exits 0, so the start check passes.
const BROWSER_PAGE: &str = "#!/usr/bin/env bash\necho '<html><head></head><body></body></html>'\n";
pub const LOCAL_CONFIG_DIR: &str = "/synthetic/local-claude-config";
pub const INSTRUCTIONS: &str = "Improve one topic.\n";
/// The rule file that the commit's `skills/` tree holds, and the line that only a later origin/main holds.
pub const PINNED_RULE: &str = "A rule of the pinned commit.\n";
pub const NEWER_RULE: &str = "A rule that only origin/main holds.\n";
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
  printf 'To authenticate, visit: https://login.tailscale.com/a/synthetic-approval\n' >&2
  sleep 30
  exit 255
fi
if [[ -n ${SSH_UNREACHABLE_FILE:-} && -e $SSH_UNREACHABLE_FILE ]]; then
  cat -- "$SSH_UNREACHABLE_FILE" >&2
  exit 255
fi
if [[ ${SSH_DENY_PUBLICKEY:-0} == 1 ]]; then
  printf 'synthetic-user@improve.example.ts.net: Permission denied (publickey).\n' >&2
  exit 255
fi
if [[ ${SSH_FAIL_SEND:-0} == 1 && $remote == *'umask 077; mkdir -p'* ]]; then
  printf 'ssh: connect to host synthetic-improve-host port 22: Connection timed out\n' >&2
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
# A dropped connection carries the round's start: the call returns 255 once the trigger file appears, and the remote command
# keeps running with its output and error going nowhere, as on a closed connection.
if [[ -n ${SSH_DROP_START_AFTER:-} && $remote == *'remote-runner.sh '* && $remote != *' --'* ]]; then
  { cat | env -i "HOME=$SSH_REMOTE_HOME" "PATH=$SSH_REMOTE_PATH" "${environment[@]}" bash -c "$remote"; } 2>>"$SSH_ARG_LOG.dropped" | cat &
  pump=$!
  while [[ ! -e $SSH_DROP_START_AFTER ]] && kill -0 "$pump" 2>/dev/null; do
    sleep 0.05
  done
  sleep 0.5
  kill "$pump" 2>/dev/null || true
  exit 255
fi
if [[ ${SSH_DROP_BEFORE_START:-0} == 1 && $remote == *'remote-runner.sh '* && $remote != *' --'* ]]; then
  printf 'Connection reset by peer\n' >&2
  exit 255
fi
# A state query that releases the round after it answers: the round's wait ends once the launch has read a running state.
if [[ -n ${SSH_RELEASE_AFTER_STATE:-} && $remote == *'--round-state'* ]]; then
  cat | env -i "HOME=$SSH_REMOTE_HOME" "PATH=$SSH_REMOTE_PATH" "${environment[@]}" bash -c "$remote"
  status=$?
  : >"$SSH_RELEASE_AFTER_STATE"
  exit "$status"
fi
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
      [[ -e $FAKE_CLAUDE_BARRIER/test_perf && -e $FAKE_CLAUDE_BARRIER/test_ux ]] && break
      sleep 0.1
    done
    [[ -e $FAKE_CLAUDE_BARRIER/test_perf && -e $FAKE_CLAUDE_BARRIER/test_ux ]] || {
      printf 'both rounds did not reach the shared mock home barrier\n' >&2
      exit 92
    }
  fi
  if [[ ! -s $FAKE_MOCK_HOME_DIR/manifest.json ]]; then
    printf 'the shared mock home changed while the round was running\n' >&2
    exit 93
  fi
fi
# A process the round leaves behind: it outlives the agent, as a test run's child does, and the
# stand-in cgroup lists it, so ending the scope takes it.
if [[ -n ${FAKE_CLAUDE_LEFTOVER_CGROUP:-} ]]; then
  sleep 30 &
  printf '%s\n' "$!" >"$FAKE_CLAUDE_LEFTOVER_PID"
  printf '%s\n' "$!" >>"$FAKE_CLAUDE_LEFTOVER_CGROUP/cgroup.procs"
fi
printf '%s\n' "$@" >"$FAKE_CLAUDE_ARGV"
# The record keeps every key out of the evidence: a real claude holds its environment in memory only, so the record
# carries each key's digest in its place, and `unset` for a key that the environment does not hold.
env | grep -v -e '^ANTHROPIC_AUTH_TOKEN=' -e '^CLAUDE_CODE_OAUTH_TOKEN=' >"$FAKE_CLAUDE_ENV"
for name in ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_OAUTH_TOKEN; do
  if [[ -n ${!name+x} ]]; then
    printf '%s_SHA256=%s\n' "$name" "$(printf '%s' "${!name}" | sha256sum | cut -d' ' -f1)"
  else
    printf '%s_SHA256=unset\n' "$name"
  fi
done >>"$FAKE_CLAUDE_ENV"
if [[ -n ${FAKE_CLAUDE_STDIN:-} ]]; then
  cat >"$FAKE_CLAUDE_STDIN"
fi
# The instructions file that the round received, copied as the runner left it in the launch directory.
if [[ -n ${FAKE_CLAUDE_INSTRUCTIONS:-} ]]; then
  previous=
  for argument in "$@"; do
    [[ $previous != --append-system-prompt-file ]] || cp -- "$argument" "$FAKE_CLAUDE_INSTRUCTIONS"
    previous=$argument
  done
fi
if [[ -n ${FAKE_CLAUDE_INFLIGHT:-} && -e $IMPROVE_LAUNCH_DIR/inflight-files.txt ]]; then
  cp -- "$IMPROVE_LAUNCH_DIR/inflight-files.txt" "$FAKE_CLAUDE_INFLIGHT"
fi
# The in-flight notice the round's final report must carry.
if [[ -n ${FAKE_CLAUDE_INFLIGHT_NOTICES:-} && -e $IMPROVE_LAUNCH_DIR/inflight-files-notices.txt ]]; then
  cp -- "$IMPROVE_LAUNCH_DIR/inflight-files-notices.txt" "$FAKE_CLAUDE_INFLIGHT_NOTICES"
fi
# The probe answer that the round's environment names, copied as the serve trial reads it.
if [[ -n ${CHARLIEBOT_TRIAL_PROBE_ANSWER-} ]]; then
  cp -- "$CHARLIEBOT_TRIAL_PROBE_ANSWER" "$FAKE_CLAUDE_PROBE"
fi
# A dropped connection: the round prints its first output, then the trigger file drops the connection that carries it. The
# unreachable file, when set, makes every later call to the host fail the way an unreachable host does.
if [[ -n ${FAKE_CLAUDE_DROP:-} ]]; then
  printf 'first part\n'
  if [[ -n ${FAKE_CLAUDE_UNREACHABLE:-} ]]; then
    printf 'Timeout, server not responding.\n' >"$FAKE_CLAUDE_UNREACHABLE"
  fi
  : >"$FAKE_CLAUDE_DROP"
fi
if [[ -n ${FAKE_CLAUDE_WAIT_FOR:-} ]]; then
  for _ in $(seq 1 600); do
    [[ -e $FAKE_CLAUDE_WAIT_FOR ]] && break
    sleep 0.1
  done
fi
# After the drop, the round's final output and its evidence, which the launch copies back once it has the round's end.
if [[ -n ${FAKE_CLAUDE_DROP:-} ]]; then
  printf 'final part\n'
  printf 'the round result\n' >evidence/round-result.txt
fi
# The round's review: the command the round's agent runs, from the review's scratch directory, with the launch's review
# directory and no model, option or key.
if [[ -n ${FAKE_CLAUDE_REVIEW_TASK:-} ]]; then
  (cd "$FAKE_CLAUDE_REVIEW_DIR" && charliebot backend run --launch "$IMPROVE_LAUNCH_DIR/review" \
    --task-file "$FAKE_CLAUDE_REVIEW_TASK")
fi
# One write of the round: the gh command a test sets, run in the round's environment, so the round's write checks
# see it as the member's own call does.
if [[ -n ${FAKE_CLAUDE_COMMAND:-} ]]; then
  bash -c "$FAKE_CLAUDE_COMMAND"
  exit "$?"
fi
exit "${FAKE_CLAUDE_EXIT:-0}"
"#;

/// The output of `codex debug prompt-input` for a codex that lists no skills: its skills section, with the marker that
/// the round's start check reads, and no entries. The start check fails a discovery without that marker.
pub const NO_SKILLS_DISCOVERY: &str = r#"[{"role":"developer","content":[{"type":"input_text","text":"<skills_instructions>\n## Skills\n### Skill roots\n### Available skills\n</skills_instructions>"}]}]"#;

/// Fake codex: record the `CODEX_HOME` it runs with, its environment, its arguments, and the `auth.json` it finds there
/// with that file's mode, outside the round's boundary.
const CODEX: &str = r#"# Fake codex: record the CODEX_HOME it runs with, the environment and arguments it receives and its login file.
# The skill discovery of a codex round asks `codex debug prompt-input`: the answer lists no skills, and it is no run.
if [[ ${1-} == debug ]]; then
  printf '%s\n' '@NO_SKILLS_DISCOVERY@'
  exit 0
fi
printf '%s\n' "$0" >>"$FAKE_CLI_RUNS"
printf 'CODEX_HOME=%s\n' "${CODEX_HOME-unset}" >"$FAKE_CODEX_ENV"
env | grep -v -e '^CODEX_HOME=' >>"$FAKE_CODEX_ENV"
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

/// The fake codex as a CLI script with its `--version` line; its `debug prompt-input` answer lists no skills.
fn codex_script(line: &str) -> String {
  cli_script(&CODEX.replace("@NO_SKILLS_DISCOVERY@", NO_SKILLS_DISCOVERY), line)
}

/// The SHA-256 of a file, in the form the vendor publishes.
fn sha256_file(path: &Path) -> String {
  let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
  format!("{:x}", Sha256::digest(bytes))
}

/// The launching host's `charliebot`: `backend resolve --call improve_review` prints the answer that FAKE_RESOLVE_ANSWER
/// names. Without that file the call class is unset, and the command fails as the CLI fails for an unset key.
/// `release-helper endpoint-option` prints FAKE_PROBE_ANSWER, the endpoint member of `backends.calls.probe`, which the
/// launch ships.
const RESOLVE_CHARLIEBOT: &str = r#"# Fake charliebot on the launching host: answer the review resolve and the probe endpoint from their fixtures, or refuse
# as an unset call class.
if [[ ${1-} == release-helper && ${2-} == endpoint-option ]]; then
  cat -- "$FAKE_PROBE_ANSWER"
  exit 0
fi
if [[ ${1-} == backend && ${2-} == resolve && ${3-} == --call && ${4-} == improve_review ]]; then
  if [[ -r $FAKE_RESOLVE_ANSWER ]]; then
    cat -- "$FAKE_RESOLVE_ANSWER"
    exit 0
  fi
  printf 'backend resolve: backends.calls.improve_review is not set in config.yaml\n' >&2
  exit 1
fi
printf 'the launcher ran an unexpected charliebot command: %s\n' "$*" >&2
exit 1
"#;

/// The improve host's `charliebot`: a `backend run` records its arguments, its working directory, the digest of each
/// variable it holds and, with `--launch`, the mode and the digest of each value of the launch's login. Then it waits for
/// FAKE_REVIEW_RELEASE when the test sets one, and records its process id first. Any other command exits 0, as the
/// round's hooks expect. The digests keep every value out of the records.
const REVIEW_CHARLIEBOT: &str = r#"# Fake charliebot on the improve host: record a backend run, then hold it when the test asks.
if [[ ${1-} != backend || ${2-} != run ]]; then
  exit 0
fi
printf '%s\n' "$@" >"$FAKE_REVIEW_ARGV"
pwd >"$FAKE_REVIEW_CWD"
while IFS= read -r name; do
  printf '%s=%s\n' "$name" "$(printf '%s' "${!name}" | sha256sum | cut -d' ' -f1)"
done < <(compgen -e) >"$FAKE_REVIEW_ENV"
if [[ -n ${FAKE_REVIEW_LOGIN:-} ]]; then
  {
    printf 'mode=%s\n' "$(stat -c %a -- "$4/login")"
    while IFS='=' read -r name value; do
      printf '%s=%s\n' "$name" "$(printf '%s' "$value" | sha256sum | cut -d' ' -f1)"
    done <"$4/login"
  } >"$FAKE_REVIEW_LOGIN"
fi
if [[ -n ${FAKE_REVIEW_RELEASE:-} ]]; then
  printf '%s\n' "$$" >"$FAKE_REVIEW_PID"
  : >"$FAKE_REVIEW_STARTED"
  for _ in $(seq 1 600); do
    [[ -e $FAKE_REVIEW_RELEASE ]] && break
    sleep 0.05
  done
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
    printf 'generate %s %s %s\n' "$2" "${3-}" "${4-}" >>"$FAKE_MOCK_HOME_LOG"
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

/// The stub `du` of the runner's cache trim. Without `FAKE_DU_MIB` it is the real `du`. With it, the stub reports the
/// cache's size in MiB for each call, one value per call with the last value repeating, and logs each call to
/// `FAKE_TRIM_LOG` as `started` or `unstarted`, by whether the round's `started` marker exists when the call runs.
pub const DU_STUB: &str = r#"set -euo pipefail
if [[ -z ${FAKE_DU_MIB-} ]]; then
  exec /usr/bin/du "$@"
fi
count=0
if [[ -f $FAKE_DU_CALLS ]]; then
  count=$(<"$FAKE_DU_CALLS")
fi
count=$((count + 1))
printf '%s\n' "$count" >"$FAKE_DU_CALLS"
IFS=, read -r -a sizes <<<"$FAKE_DU_MIB"
index=$((count - 1))
if (( index >= ${#sizes[@]} )); then
  index=$((${#sizes[@]} - 1))
fi
if [[ -e $FAKE_STARTED_MARKER ]]; then
  printf 'du started\n' >>"$FAKE_TRIM_LOG"
else
  printf 'du unstarted\n' >>"$FAKE_TRIM_LOG"
fi
printf '%s\t%s\n' "${sizes[index]}" "${@: -1}"
"#;

/// The stub `df` of the runner's free-space floor. Without `FAKE_DF_MIB` it reports 1 TiB free, so no test depends on
/// the disk of the host it runs on. With it, the free space in MiB comes one value per call, the last value repeating.
/// `FAKE_DF_CALLS` counts the calls when it is set, and `FAKE_TRIM_LOG` logs each call as `df started` or `df unstarted`
/// as `DU_STUB` does. The output is the `df -P -k` layout.
pub const DF_STUB: &str = r#"set -euo pipefail
IFS=, read -r -a sizes <<<"${FAKE_DF_MIB:-1048576}"
index=0
if [[ -n ${FAKE_DF_CALLS-} ]]; then
  count=0
  if [[ -f $FAKE_DF_CALLS ]]; then
    count=$(<"$FAKE_DF_CALLS")
  fi
  count=$((count + 1))
  printf '%s\n' "$count" >"$FAKE_DF_CALLS"
  index=$((count - 1))
fi
if (( index >= ${#sizes[@]} )); then
  index=$((${#sizes[@]} - 1))
fi
if [[ -n ${FAKE_TRIM_LOG-} ]]; then
  if [[ -e $FAKE_STARTED_MARKER ]]; then
    printf 'df started\n' >>"$FAKE_TRIM_LOG"
  else
    printf 'df unstarted\n' >>"$FAKE_TRIM_LOG"
  fi
fi
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/trim 1 1 %s 1%% /\n' "$((sizes[index] * 1024))"
"#;

const SYSTEMD_RUN: &str = r#"# Fake systemd-run: record the call, then run the command after `--`. An already-loaded unit
# refuses the start, unless a fake systemctl stop has ended that unit first. A stand-in cgroup
# (FAKE_SCOPE_CGROUP) is the scope's process list: the scope starts with an empty list, as a real
# scope does, and the processes that join it append their pids as a real cgroup lists them.
printf '%s\n' "$*" >>"$FAKE_SYSTEMD_RUN_LOG"
if [[ ${FAKE_SYSTEMD_RUN_EXISTING:-0} == 1 && ! -e $FAKE_SCOPE_STOPPED ]]; then
  printf 'Failed to start transient scope unit: Unit improve-test_perf.scope was already loaded or has a fragment file.\n' >&2
  exit 1
fi
if [[ -n ${FAKE_SCOPE_CGROUP:-} ]]; then
  mkdir -p -- "$FAKE_SCOPE_CGROUP"
  : >"$FAKE_SCOPE_CGROUP/cgroup.procs"
  rm -f -- "$FAKE_SCOPE_STOPPED"
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

const SYSTEMCTL: &str = r#"# Fake systemctl: record the call, answer is-failed and is-active from the scope's state and its
# stand-in cgroup, report that cgroup's path, and end the scope on a stop: the scope's processes
# are killed and its list is emptied, as the kernel's cgroup kill does.
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
    [[ -e $FAKE_SCOPE_STOPPED ]] && exit 3
    [[ -n ${FAKE_SCOPE_CGROUP:-} && -d $FAKE_SCOPE_CGROUP ]] && exit 0
    [[ ${FAKE_SCOPE_STATE:-} == active ]] && exit 0
    exit 3
    ;;
  *"show"*"ControlGroup"*)
    # The runner reads the unit's ControlGroup under the kernel's cgroup root. A stand-in lives in
    # the test sandbox, outside that root, so its path climbs out of the root to reach it.
    if [[ -n ${FAKE_SCOPE_CGROUP:-} ]]; then
      printf '/%s%s\n' "$(printf '../%.0s' {1..8})" "${FAKE_SCOPE_CGROUP#/}"
    fi
    ;;
  *"stop improve-"*)
    : >"$FAKE_SCOPE_STOPPED"
    if [[ -n ${FAKE_SCOPE_CGROUP:-} && -r $FAKE_SCOPE_CGROUP/cgroup.procs ]]; then
      while read -r pid; do
        [[ $pid == "$$" ]] && continue
        kill -KILL "$pid" 2>/dev/null || true
      done <"$FAKE_SCOPE_CGROUP/cgroup.procs"
      : >"$FAKE_SCOPE_CGROUP/cgroup.procs"
    fi
    ;;
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
  /// Where the fake claude copies the dropped worktree notices, inside the round's boundary.
  pub claude_inflight_notices: PathBuf,
  /// Where the fake claude copies its standard input when a test asks for it, inside the round's boundary.
  pub claude_stdin: PathBuf,
  /// Where the improve host's fake `charliebot backend run` writes its arguments, its directory and each variable's
  /// digest, inside the round's boundary.
  pub review_argv: PathBuf,
  pub review_cwd: PathBuf,
  pub review_env: PathBuf,
  /// The answer that the launching host's fake `backend resolve --call improve_review` prints. The file's absence
  /// makes the call class unset.
  pub resolve_answer: PathBuf,
  /// The answer that the launching host's fake `backend resolve --call probe` prints.
  pub probe_answer: PathBuf,
  /// Where the fake claude copies the probe answer that its round's environment names, inside the round's boundary.
  pub claude_probe: PathBuf,
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
  /// The file that drops the connection carrying the round's start, and the round's wait for its release.
  pub drop_trigger: PathBuf,
  pub round_release: PathBuf,
  /// The file that makes the host unreachable for every later call.
  pub host_unreachable: PathBuf,
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
    for name in [
      "remote-launch.sh",
      "remote-runner.sh",
      "round-cargo.sh",
      "inflight-files.sh",
      "clone-check.sh",
      "codex-skill-roots.sh",
      "target-cache.sh",
      "member_kinds.txt",
      "gh-shim.sh",
      "github-post.sh",
      "side-record.sh",
      "code-style-rules.sh",
      "moved-references.sh",
      "skill-reads.sh",
    ] {
      harness::copy_file(&harness::repo_root().join("dev/improve").join(name), &scripts.join(name));
    }
    // The runner installs the round's gh shim and its scanner from the commit's objects, and the round's kit is the
    // commit's dev/ tree, so the commit carries the posting script and the scripts it sources. Its clone check requires
    // the clone's pre-push hook. The commit carries them as a clone of the real repository does.
    let hooks = sandbox.dir("repo/dev/git-hooks");
    harness::write_executable(
      &hooks.join("pre-push"),
      std::fs::read(harness::repo_root().join("dev/git-hooks/pre-push")).unwrap(),
    );
    harness::copy_file(
      &harness::repo_root().join("dev/check-skills-host-leak.sh"),
      &repo.join("dev/check-skills-host-leak.sh"),
    );
    harness::copy_file(&harness::repo_root().join("dev/leak-patterns.sh"), &repo.join("dev/leak-patterns.sh"));
    harness::copy_file(&harness::repo_root().join("dev/push-text-lists.sh"), &repo.join("dev/push-text-lists.sh"));
    let local_home = sandbox.dir("local");
    let remote_home = sandbox.dir("remote");
    // The round's browser is the pinned build's Chrome under the runner's home, which the start check runs once.
    let browser_dir = sandbox.dir(&format!("remote/.cache/ms-playwright/{BROWSER_BUILD}/{BROWSER_LAYOUT}"));
    harness::write_executable(&browser_dir.join("chrome"), BROWSER_PAGE);
    let profile_home = sandbox.dir("profile");
    let git_env: Env = IDENTITY.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect();
    git(&sandbox, &git_env, &repo, &["init", "-q", "-b", "main"]);
    // The round loads the rule files of the commit's skills/ tree, so the commit carries one of them.
    let genres = sandbox.dir("repo/skills/writing-style/genres");
    fs::write(genres.join("code.md"), PINNED_RULE).expect("the commit's rule file");
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
    sandbox.stub("charliebot", RESOLVE_CHARLIEBOT);

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
    // A section before the review credential, so a test that reads the first api_key line reads the wrong key.
    fs::write(
      profile_home.join("credentials.yaml"),
      format!("synthetic-decoy:\n  api_key: synthetic-decoy-key\n{REVIEW_CREDENTIAL}:\n  api_key: {REVIEW_KEY}\n"),
    )
    .expect("the profile home's credentials.yaml");

    let round_root = sandbox.dir(&format!("remote/improve/{direction}"));
    sandbox.dir("remote/improve/home");
    // The direction clone is a clone of its origin, a bare repository named charlie-bot as the protocol's clones are.
    // The origin's main carries the commit's pre-push hook, so the clone's tree has the hook the runner's clone check
    // requires, and the runner's sync fetches from this origin. A hook the clone held untracked would block that sync's
    // checkout, because git refuses to overwrite an untracked file.
    let direction_origin = sandbox.dir("remotes/charlie-bot.git");
    let direction_origin_text = direction_origin.display().to_string();
    git(&sandbox, &git_env, &repo, &["clone", "-q", "--bare", ".", &direction_origin_text]);
    git(&sandbox, &git_env, &round_root, &["clone", "-q", &direction_origin_text, "charlie-bot"]);
    let clone = round_root.join("charlie-bot");
    git(&sandbox, &git_env, &clone, &["config", "core.hooksPath", "dev/git-hooks"]);
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
    write_executable(&remote_bin.join("codex"), codex_script(HOST_CODEX_LINE));
    // The runner's cache trim reads through `du` and `df`. These stubs stand first on the remote PATH, ahead of the host's.
    let trim_bin = sandbox.dir("trim-bin");
    write_executable(&trim_bin.join("du"), format!("#!/usr/bin/env bash\n{DU_STUB}"));
    write_executable(&trim_bin.join("df"), format!("#!/usr/bin/env bash\n{DF_STUB}"));
    let vendor_root = sandbox.dir("vendor");
    let curl_log = sandbox.root().join("curl.log");
    let cli_runs = sandbox.root().join("cli-runs.log");
    for (name, body) in [("curl", CURL), ("systemd-run", SYSTEMD_RUN), ("systemctl", SYSTEMCTL), ("cargo", CARGO)] {
      write_executable(&remote_bin.join(name), format!("#!/usr/bin/env bash\n{body}"));
    }
    // The round tools of this commit are already built, so the runner skips its build for them.
    let runner_bin = sandbox.dir("remote/improve/runner/bin");
    for (name, body) in [("charliebot", REVIEW_CHARLIEBOT), ("improve-sandbox", SANDBOX), ("mock-home", MOCK_HOME)] {
      write_executable(&runner_bin.join(format!("{name}-{commit}")), format!("#!/usr/bin/env bash\n{body}"));
    }

    let world = Self {
      _tmp_root: tmp_root,
      direction,
      ssh_args: sandbox.root().join("ssh-arguments.log"),
      claude_argv: evidence.join("claude-argv.txt"),
      claude_env: evidence.join("claude-env.txt"),
      claude_inflight: evidence.join("claude-inflight.txt"),
      claude_inflight_notices: evidence.join("claude-inflight-notices.txt"),
      claude_stdin: evidence.join("claude-stdin.txt"),
      review_argv: evidence.join("review-argv.txt"),
      review_cwd: evidence.join("review-cwd.txt"),
      review_env: evidence.join("review-env.txt"),
      resolve_answer: sandbox.root().join("resolve-answer.json"),
      probe_answer: sandbox.root().join("probe-answer.json"),
      claude_probe: evidence.join("claude-probe.json"),
      codex_env: sandbox.root().join("codex-env.txt"),
      codex_argv: sandbox.root().join("codex-argv.txt"),
      codex_auth: sandbox.root().join("codex-auth.json"),
      codex_auth_mode: sandbox.root().join("codex-auth-mode.txt"),
      claude_login_dir: sandbox.dir("login/claude"),
      systemctl_log: sandbox.root().join("systemctl.log"),
      systemd_run_log: sandbox.root().join("systemd-run.log"),
      scope_stopped: sandbox.root().join("scope-stopped"),
      drop_trigger: sandbox.root().join("drop-trigger"),
      round_release: sandbox.root().join("round-release"),
      host_unreachable: sandbox.root().join("host-unreachable"),
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
    world.write_review_answer(GATEWAY_REVIEW_TYPE, 0);
    fs::write(&world.probe_answer, SYNTHETIC_PROBE_ANSWER).expect("the probe answer");
    world
  }

  /// Write the answer that `backends.calls.improve_review` resolves to, for a review option of `option_type`, as the server
  /// prints it: the option's record with no key. A subscription option also names its account directory, and that
  /// directory's login file carries `REVIEW_LOGIN_TOKEN`, expiring at `expires_at_ms` (milliseconds since the epoch).
  pub fn write_review_answer(&self, option_type: &str, expires_at_ms: i64) {
    let mut option = serde_json::json!({"id": SYNTHETIC_REVIEW_ID, "type": option_type, "model": REVIEW_MODEL});
    let mut answer = serde_json::json!({});
    if option_type == GATEWAY_REVIEW_TYPE {
      option["api_base"] = REVIEW_API_BASE.into();
      option["context_window"] = REVIEW_CONTEXT_WINDOW.into();
      option["credential"] = REVIEW_CREDENTIAL.into();
    }
    answer["option"] = option;
    if option_type == SUBSCRIPTION_REVIEW_TYPE {
      let account = self.sandbox.dir("login/review-account");
      let login = serde_json::json!({
        "claudeAiOauth": {
          "accessToken": REVIEW_LOGIN_TOKEN,
          "refreshToken": LOGIN_REFRESH_TOKEN,
          "expiresAt": expires_at_ms,
          "scopes": ["user:inference"],
        }
      });
      fs::write(account.join(".credentials.json"), login.to_string()).expect("the review account's login file");
      answer["claude_config_dir"] = account.display().to_string().into();
    }
    fs::write(&self.resolve_answer, answer.to_string()).expect("the resolve answer");
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
        write_executable(&bin.join("codex"), codex_script(reported));
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

  /// The stand-in cgroup of one scope: a directory whose `cgroup.procs` lists the processes the scope holds, as a
  /// real scope's cgroup does. The scope exists while that directory does; the stub systemd-run makes it, a test
  /// makes it for a scope a previous round left behind, and the stub systemctl reports it as the unit's
  /// ControlGroup.
  pub fn scope_cgroup(&self, name: &str) -> PathBuf {
    self.sandbox.root().join("scope").join(name)
  }

  /// The processes of an active scope that holds no round: `count` stand-in processes that a finished round left
  /// in it, listed in the scope's stand-in cgroup. The caller kills any that a stop left behind.
  pub fn spawn_scope_leftovers(&self, cgroup: &Path, count: usize) -> Vec<Child> {
    fs::create_dir_all(cgroup).expect("the stand-in cgroup's directory");
    let children: Vec<Child> = (0..count)
      .map(|index| {
        Command::new("sleep")
          .arg("60")
          .stdin(Stdio::null())
          .spawn()
          .unwrap_or_else(|error| panic!("scope leftover {index} starts: {error}"))
      })
      .collect();
    let listed: String = children.iter().map(|child| format!("{}\n", child.id())).collect();
    fs::write(cgroup.join("cgroup.procs"), listed).expect("the stand-in cgroup's process list");
    children
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
    env.insert(
      "PATH".to_string(),
      format!("{}:{}:/usr/local/bin:/usr/bin:/bin", self.trim_bin().display(), self.sandbox.bin().display()),
    );
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
      format!(
        "{}:{}:/usr/local/bin:/usr/bin:/bin",
        self.trim_bin().display(),
        self.remote_home.join(".local/bin").display()
      ),
    );
    env.insert("FAKE_CLAUDE_ARGV".to_string(), self.claude_argv.display().to_string());
    env.insert("FAKE_CLAUDE_ENV".to_string(), self.claude_env.display().to_string());
    env.insert("FAKE_CLAUDE_INFLIGHT".to_string(), self.claude_inflight.display().to_string());
    env.insert("FAKE_CLAUDE_INFLIGHT_NOTICES".to_string(), self.claude_inflight_notices.display().to_string());
    env.insert("FAKE_CODEX_ENV".to_string(), self.codex_env.display().to_string());
    env.insert("FAKE_CODEX_ARGV".to_string(), self.codex_argv.display().to_string());
    env.insert("FAKE_CODEX_AUTH".to_string(), self.codex_auth.display().to_string());
    env.insert("FAKE_CODEX_AUTH_MODE".to_string(), self.codex_auth_mode.display().to_string());
    env.insert("IMPROVE_TOKEN_MIN_SECONDS".to_string(), TOKEN_MIN_SECONDS.to_string());
    env.insert("FAKE_RESOLVE_ANSWER".to_string(), self.resolve_answer.display().to_string());
    env.insert("FAKE_PROBE_ANSWER".to_string(), self.probe_answer.display().to_string());
    env.insert("FAKE_CLAUDE_PROBE".to_string(), self.claude_probe.display().to_string());
    env.insert("FAKE_REVIEW_ARGV".to_string(), self.review_argv.display().to_string());
    env.insert("FAKE_REVIEW_CWD".to_string(), self.review_cwd.display().to_string());
    env.insert("FAKE_REVIEW_ENV".to_string(), self.review_env.display().to_string());
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

  /// The directory of the `du` and `df` stubs that the runner's cache trim reads through.
  pub fn trim_bin(&self) -> PathBuf {
    self.sandbox.root().join("trim-bin")
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

  /// The pinned build's directory under the runner's home, where the round's start check looks for its Chrome.
  pub fn browser_build_dir(&self) -> PathBuf {
    self.remote_home.join(".cache/ms-playwright").join(BROWSER_BUILD)
  }

  /// The Chrome that the round's start check runs, and that the round exports as `IMPROVE_BROWSER`.
  pub fn browser_path(&self) -> PathBuf {
    self.browser_build_dir().join(BROWSER_LAYOUT).join("chrome")
  }

  /// The probe answer that the round's environment named, as the round's agent read it from that file.
  pub fn claude_probe_answer(&self) -> String {
    fs::read_to_string(&self.claude_probe).expect("the fake claude copied the probe answer").trim_end().to_string()
  }

  /// Whether the improve host's fake `charliebot backend run` ran at all.
  pub fn review_ran(&self) -> bool {
    self.review_argv.exists()
  }

  pub fn review_arguments_received(&self) -> Vec<String> {
    record_lines(&self.review_argv)
  }

  /// The working directory that the review ran in.
  pub fn review_directory_received(&self) -> String {
    fs::read_to_string(&self.review_cwd).expect("the fake review recorded its directory").trim_end().to_string()
  }

  /// The digest of each variable that the review's environment holds, by name. A value is never in the record.
  pub fn review_environment(&self) -> Env {
    record_environment(&self.review_env)
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

  /// A launch directory the runner reads, created as the launcher's payload creates it: the member kind list, the clone
  /// check, and the review backend's and the probe's answers sit in it.
  pub fn stage_launch(&self, launch_id: &str) -> PathBuf {
    let launch = self.remote_home.join("improve/launch").join(launch_id);
    fs::create_dir_all(launch.join("review")).expect("the launch's review directory");
    fs::create_dir_all(launch.join("probe")).expect("the launch's probe directory");
    harness::copy_file(&self.repo.join("dev/improve/member_kinds.txt"), &launch.join("member_kinds.txt"));
    harness::copy_file(&self.repo.join("dev/improve/clone-check.sh"), &launch.join("clone-check.sh"));
    harness::copy_file(&self.repo.join("dev/improve/codex-skill-roots.sh"), &launch.join("codex-skill-roots.sh"));
    harness::copy_file(&self.repo.join("dev/improve/target-cache.sh"), &launch.join("target-cache.sh"));
    harness::copy_file(&self.resolve_answer, &launch.join("review/option.json"));
    harness::copy_file(&self.probe_answer, &launch.join("probe/option.json"));
    // The launcher names the revision of the kit and of the skills the runner fetches, which is the commit its charliebot
    // is built from.
    fs::write(launch.join("kit-revision"), format!("{}\n", self.commit)).expect("the launch's kit revision");
    fs::write(launch.join("skills-revision"), format!("{}\n", self.commit)).expect("the launch's skills revision");
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
