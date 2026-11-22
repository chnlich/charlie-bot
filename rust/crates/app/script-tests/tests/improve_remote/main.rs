//! One improve run: `dev/improve/remote-launch.sh` on this host. A round then runs
//! `dev/improve/remote-runner.sh` on the remote host. A fake `ssh` records its arguments and runs the remote side
//! locally. Fake `claude`, `codex`, `systemd-run`, `systemctl` and `cargo` record what they receive, so each test checks
//! the run's contract without a network, a server or a real remote host.
//!
//! The tests that assert a bound on time measure it: the approval preflight and the heartbeat stop must both finish
//! inside five seconds.

mod world;

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use charliebot_shell_harness as harness;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use world::{
  CODEX_REFRESH_TOKEN, DIRECTION, GATEWAY_TOKEN, LOGIN_REFRESH_TOKEN, LOGIN_TOKEN, REVIEW_API_BASE, REVIEW_CREDENTIAL,
  REVIEW_KEY, REVIEW_MODEL, World,
};

/// The prompt the server writes to the launcher's standard input for a plain-text run.
const PROMPT: &str = "Improve one topic in the performance direction.\nReply with the word OK.\n";

fn stderr(run: &harness::Run) -> String {
  run.stderr.clone()
}

/// Whether the list names `path`, bare or with the branch that carries it.
fn lists_path(list: &str, path: &str) -> bool {
  list.lines().any(|line| line == path || line.starts_with(&format!("{path}\t")))
}

/// Whether `condition` holds within `limit`, polled until it does.
fn wait_until(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
  let start = Instant::now();
  while start.elapsed() < limit {
    if condition() {
      return true;
    }
    std::thread::sleep(Duration::from_millis(20));
  }
  condition()
}

#[test]
#[ignore = "integration"]
fn the_round_receives_claudes_arguments_with_the_launch_directory_paths() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());

  let received = world.claude_arguments_received();
  let position = received
    .iter()
    .position(|argument| argument == "--append-system-prompt-file")
    .unwrap_or_else(|| panic!("no --append-system-prompt-file in {received:?}"));
  let copied = std::path::Path::new(&received[position + 1]);
  assert!(copied.starts_with(world.remote_home.join("improve/launch")), "{received:?}");
  assert_eq!(copied.file_name().map(|name| name.to_string_lossy().into_owned()), Some("instructions.md".to_string()));
  assert_ne!(received[position + 1], world.instructions.display().to_string());
  assert!(world.launch_directory().is_none(), "the runner left its launch directory behind");
  assert!(
    world.ssh_arguments().contains(&world.commit),
    "the runner was sent no CLI commit: {}",
    world.ssh_arguments()
  );
  assert!(received.iter().all(|argument| argument != "--mcp-config"), "{received:?}");
  assert!(received.windows(2).any(|pair| pair == ["--output-format", "stream-json"]), "{received:?}");
  assert!(
    received.windows(2).any(|pair| pair == ["--settings", "{\"disableClaudeAiConnectors\":true}"]),
    "{received:?}"
  );
}

#[test]
#[ignore = "integration"]
fn the_prompt_on_the_launchers_standard_input_reaches_the_remote_claude() {
  let world = World::new();
  let prompt = world.sandbox.root().join("prompt.txt");
  fs::write(&prompt, PROMPT).expect("the prompt");
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_STDIN".to_string(), world.claude_stdin.display().to_string());
  // The launcher's standard input is a file here, as the server's pipe is in production. Every process that inherits
  // it shares one offset, so a process that reads the prompt before the round's own ssh leaves that ssh nothing.
  let stdin = fs::File::open(&prompt).expect("open the prompt");
  let mut command = world.launcher_command(&env);
  let run = harness::run_with_stdin(&mut command, Stdio::from(stdin));
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let text = fs::read_to_string(&world.claude_stdin).expect("the fake claude recorded its standard input");
  assert_eq!(text, PROMPT, "the remote claude did not receive the prompt:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_round_worker_whose_parent_is_a_cron_session_still_runs_over_ssh() {
  let world = World::new();
  // The worker's metadata names PARENT_ID as its task_parent_id, and the cron task file binds
  // PARENT_ID. This run is therefore a round even though a cron session is one link away.
  let approval = world.sandbox.root().join("approval-needed");
  fs::write(&approval, "1\n").expect("the approval marker");
  let mut env = world.env();
  env.insert("SSH_APPROVAL_FILE".to_string(), approval.display().to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(world.ssh_arguments().contains("\ntrue\n"), "the preflight did not reach ssh:\n{}", world.ssh_arguments());
  assert!(!world.claude_ran(), "the round ran before its preflight passed");
}

/// The in-flight list's run from `cwd`.
fn run_inflight_list_from(world: &World, env: &harness::Env, cwd: &Path) -> harness::Run {
  let mut command = harness::command("bash", env);
  command.arg(world.repo.join("dev/improve/inflight-files.sh")).arg("improve/perf/").current_dir(cwd);
  harness::run(&mut command)
}

fn run_launcher_from(world: &World, env: &harness::Env, cwd: &Path, arguments: &[String]) -> harness::Run {
  let mut command = harness::command("bash", env);
  command.arg(world.launcher()).args(arguments).current_dir(cwd);
  harness::run(&mut command)
}

#[test]
#[ignore = "integration"]
fn the_inflight_list_names_its_origin_repository_outside_a_git_checkout() {
  let world = World::new();
  let cwd = world.sandbox.root();
  assert!(!cwd.join(".git").exists(), "the test working directory is a git checkout");

  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "12\tcharliebot/task-open\tpr-only.md\n").expect("the open pull requests");

  let mut env = world.env();
  env.remove("GH_REPO");
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());
  env.insert("FAKE_GH_REPO".to_string(), world::GITHUB_REPOSITORY.to_string());
  env.insert("FAKE_GH_HOST".to_string(), "github.com".to_string());

  let run = run_inflight_list_from(&world, &env, cwd);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout, "pr-only.md\tcharliebot/task-open\n", "{}", run.describe());

  // An origin that names no GitHub repository stops the list with an error naming the condition.
  world.git(&world.repo, &["remote", "set-url", "origin", "not-a-github-origin"]);
  let run = run_inflight_list_from(&world, &env, cwd);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("cannot parse the origin"), "{}", run.describe());

  // So does a checkout whose origin cannot be read at all.
  world.git(&world.repo, &["remote", "remove", "origin"]);
  let run = run_inflight_list_from(&world, &env, cwd);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("cannot read the origin"), "{}", run.describe());

  let mut relative_profile_home = env;
  relative_profile_home.insert("CHARLIEBOT_HOME".to_string(), "profile".to_string());
  let run = run_inflight_list_from(&world, &relative_profile_home, cwd);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("CHARLIEBOT_HOME must be an absolute path"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_remote_launcher_rejects_relative_input_and_temp_paths() {
  let world = World::new();
  let cwd = world.sandbox.root();

  let mut arguments = world.agent_arguments();
  let instructions = arguments
    .iter()
    .position(|argument| argument == "--append-system-prompt-file")
    .expect("the launcher arguments name the instructions file");
  arguments[instructions + 1] = "harness_instructions.md".to_string();
  let env = world.env();
  let run = run_launcher_from(&world, &env, cwd, &arguments);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("the instructions file path must be absolute"), "{}", run.describe());

  let mut env = world.env();
  env.insert("TMPDIR".to_string(), "relative-temp".to_string());
  let arguments = world.agent_arguments();
  let run = run_launcher_from(&world, &env, cwd, &arguments);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("TMPDIR must be an absolute path"), "{}", run.describe());

  for (name, path) in [("HOME", "relative-home"), ("CHARLIEBOT_HOME", "relative-profile")] {
    let mut env = world.env();
    env.insert(name.to_string(), path.to_string());
    let run = run_launcher_from(&world, &env, cwd, &arguments);
    assert_ne!(run.code, 0, "{}", run.describe());
    assert!(run.stderr.contains(&format!("{name} must be an absolute path")), "{}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn the_remote_runner_rejects_a_relative_home() {
  let world = World::new();
  let mut env = world.env();
  env.insert("HOME".to_string(), "relative-home".to_string());
  let mut command = harness::command("bash", &env);
  command.arg(world.repo.join("dev/improve/remote-runner.sh")).args(["--prepare", "launch-id"]);

  let run = harness::run(&mut command);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("HOME must be an absolute path"), "{}", run.describe());
}

/// The SHA-256 of a key, in the form the fake agents record it.
fn digest(value: &str) -> String {
  format!("{:x}", Sha256::digest(value))
}

/// The files under `root` whose text contains `needle`, relative to `root`.
fn files_holding(world: &World, root: &Path, needle: &str) -> Vec<String> {
  world
    .tree(root)
    .into_iter()
    .filter(|path| {
      let file = root.join(path);
      file.is_file() && fs::read_to_string(&file).is_ok_and(|text| text.contains(needle))
    })
    .collect()
}

#[test]
#[ignore = "integration"]
fn the_gateway_member_gets_its_token_and_no_review_key() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let env = world.claude_environment();
  let member = digest(GATEWAY_TOKEN);
  assert_eq!(env.get("ANTHROPIC_AUTH_TOKEN_SHA256").map(String::as_str), Some(member.as_str()), "{}", run.describe());
  // The fake claude records `unset` for a key its environment does not hold, so the review key reaches no member,
  // and its tool shell, a child of the member, holds none either.
  assert_eq!(env.get("CHARLIE_CODE_API_KEY_SHA256").map(String::as_str), Some("unset"), "{}", run.describe());
  assert_eq!(env.get("TOOL_SHELL_CHARLIE_CODE_API_KEY").map(String::as_str), Some("unset"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_review_runs_charlie_code_on_the_review_options_model_and_api_base_with_no_flags() {
  let world = World::new();
  let task = world.sandbox.root().join("review-task.md");
  fs::write(&task, "Review the diff.\nPR: 1 https://example.invalid/pull/1\n").expect("the review task file");
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_REVIEW_TASK".to_string(), task.display().to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.charlie_code_ran(), "the round's review did not run charlie-code:\n{}", run.describe());

  // The review command names no model, API base or key: the review shim sets all three for its charlie-code.
  let task = task.display().to_string();
  let session_dir = world.remote_home.join("improve/perf/clc-sessions").display().to_string();
  assert_eq!(
    world.charlie_code_arguments_received(),
    ["--json", "--task-file", task.as_str(), "--session-dir", session_dir.as_str()],
    "{}",
    run.describe()
  );
  let env = world.charlie_code_environment();
  let value = |name: &str| env.get(name).map(String::as_str);
  let model = format!("openai/{REVIEW_MODEL}");
  let api_base = format!("{REVIEW_API_BASE}/v1");
  let review = digest(REVIEW_KEY);
  assert_eq!(value("CHARLIE_CODE_MODEL"), Some(model.as_str()), "{}", run.describe());
  assert_eq!(value("CHARLIE_CODE_API_BASE"), Some(api_base.as_str()), "{}", run.describe());
  assert_eq!(value("CHARLIE_CODE_API_KEY_SHA256"), Some(review.as_str()), "{}", run.describe());
}

/// The expiry, in milliseconds since the epoch, of a login whose access token lasts `hours` more hours.
fn expiry_ms(hours: i64) -> i64 {
  (world::now_seconds() + hours * 3600) * 1000
}

/// Write a launch's member-credential file as `--prepare` writes it: each record as `NAME=value` and a NUL.
fn write_member_credential(launch: &Path, records: &[(&str, &str)]) {
  let mut bytes = Vec::new();
  for (name, value) in records {
    bytes.extend_from_slice(format!("{name}={value}").as_bytes());
    bytes.push(0);
  }
  fs::write(launch.join("member-credential"), bytes).expect("the launch's member credential");
}

#[test]
#[ignore = "integration"]
fn a_subscription_claude_forwards_its_access_token_and_no_refresh_token() {
  let world = World::new();
  world.write_claude_login(expiry_ms(10));
  let run = world.run(&world.host_login_env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let env = world.claude_environment();
  let value = |name: &str| env.get(name).map(String::as_str);
  let login = digest(LOGIN_TOKEN);
  assert_eq!(value("CLAUDE_CODE_OAUTH_TOKEN_SHA256"), Some(login.as_str()), "{}", run.describe());
  assert_eq!(value("ANTHROPIC_AUTH_TOKEN_SHA256"), Some("unset"), "{}", run.describe());
  assert_eq!(value("CHARLIE_CODE_API_KEY_SHA256"), Some("unset"), "{}", run.describe());
  // The access token reaches no ssh argument and no file on either home once the round ends. The refresh token
  // reaches nothing but the login file on this host.
  let arguments = world.ssh_arguments();
  assert!(!arguments.contains(LOGIN_TOKEN), "the access token reached an ssh argument");
  assert!(!arguments.contains(LOGIN_REFRESH_TOKEN), "the refresh token reached an ssh argument");
  assert!(
    files_holding(&world, &world.local_home, LOGIN_TOKEN).is_empty(),
    "the access token reached this host's files"
  );
  assert!(files_holding(&world, &world.remote_home, LOGIN_TOKEN).is_empty(), "the access token outlived the round");
  assert!(
    files_holding(&world, &world.remote_home, LOGIN_REFRESH_TOKEN).is_empty(),
    "the refresh token reached the remote home"
  );
}

#[test]
#[ignore = "integration"]
fn a_codex_member_runs_under_its_round_codex_home_with_the_forwarded_login() {
  let world = World::new();
  let exp = world::now_seconds() + 200 * 3600;
  world.write_codex_login(exp);
  let arguments = world.codex_arguments();
  let run = harness::run(&mut world.launcher_command_for(&world.host_login_env(), &arguments));
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.codex_ran(), "the fake codex did not run:\n{}", run.describe());
  assert!(!world.claude_ran(), "a codex member ran claude:\n{}", run.describe());
  let codex_home = world.codex_home_directory().display().to_string();
  assert_eq!(world.codex_home_received(), codex_home, "{}", run.describe());
  let record = fs::read_to_string(&world.codex_env).expect("the fake codex recorded its environment");
  assert!(record.contains("\nCHARLIE_CODE_API_KEY=unset\n"), "the codex member holds the review key:\n{record}");
  assert!(
    record.contains("\nTOOL_SHELL_CHARLIE_CODE_API_KEY=unset\n"),
    "the codex tool shell holds the key:\n{record}"
  );
  assert_eq!(world.codex_arguments_received(), arguments[1..].to_vec(), "{}", run.describe());

  // The login the round's codex found: the host's auth.json with an empty refresh token, mode 600.
  let found = fs::read_to_string(&world.codex_auth).expect("the login the fake codex found");
  let mut expected: Value = serde_json::from_str(&world::codex_login_json(exp)).expect("the fixture login is JSON");
  expected["tokens"]["refresh_token"] = json!("");
  let forwarded: Value = serde_json::from_str(&found).expect("the forwarded login is JSON");
  assert!(forwarded == expected, "the forwarded Codex login has the wrong fields");
  assert!(
    !forwarded.as_object().expect("the forwarded login is an object").contains_key("OPENAI_API_KEY"),
    "the forwarded Codex login includes an unrequested API key field"
  );
  assert_eq!(fs::read_to_string(&world.codex_auth_mode).expect("the login's mode").trim(), "600");

  // The round removed its login when it ended, and the refresh token stayed on this host.
  assert!(!world.codex_home_directory().join("auth.json").exists(), "the round left its login behind");
  assert!(
    files_holding(&world, &world.remote_home, CODEX_REFRESH_TOKEN).is_empty(),
    "the refresh token reached the remote home"
  );
  assert!(!world.ssh_arguments().contains(CODEX_REFRESH_TOKEN), "the refresh token reached an ssh argument");
}

/// The stub improve-sandbox of the focused export checks: it records the digest of the member's gateway token and
/// whether the review key is set, so the assertion holds no token value. It also records the TMPDIR the scope exports.
const SCOPE_ENV_STUB: &str = r#"# Fake improve-sandbox: record the digest of the gateway token, whether the review key is set, the CODEX_HOME it runs
# with and the login file under it with that file's mode, then exit.
{
  printf 'ANTHROPIC_AUTH_TOKEN_SHA256=%s\n' "$(printf '%s' "${ANTHROPIC_AUTH_TOKEN-}" | sha256sum | cut -d' ' -f1)"
  if [[ -n ${CHARLIE_CODE_API_KEY+x} ]]; then
    printf 'CHARLIE_CODE_API_KEY=set\n'
  else
    printf 'CHARLIE_CODE_API_KEY=unset\n'
  fi
  printf 'CODEX_HOME=%s\n' "${CODEX_HOME-unset}"
  printf 'TMPDIR=%s\n' "${TMPDIR-unset}"
} >"$FAKE_SCOPE_ENV"
if [[ -n ${CODEX_HOME-} && -f $CODEX_HOME/auth.json ]]; then
  cp -- "$CODEX_HOME/auth.json" "$FAKE_SCOPE_AUTH"
  printf 'CODEX_AUTH_MODE=%s\n' "$(stat -c %a -- "$CODEX_HOME/auth.json")" >>"$FAKE_SCOPE_ENV"
fi
# A review that the agent left running: the stub charlie-code starts in the background and outlives this scope's agent.
if [[ -n ${FAKE_SCOPE_LEFT_REVIEW:-} ]]; then
  charlie-code --json >/dev/null 2>&1 </dev/null &
  for _ in $(seq 1 600); do
    [[ -e $FAKE_REVIEW_STARTED ]] && break
    sleep 0.1
  done
fi
exit 0
"#;

/// The direction of the focused export checks. Its TMPDIR name no real direction's round shares.
const FOCUS_DIRECTION: &str = "focused";
const SYMLINK_DIRECTION: &str = "symlink_review";
const SCOPE_TMPDIR_DIRECTION: &str = "scope_tmpdir";

/// The run of the runner's `--in-scope` form alone, with the stub improve-sandbox that records the gateway
/// variables and the Codex home. The first form exports the launch environment before it starts the scope, so the
/// review credential's name arrives here as `IMPROVE_REVIEW_CREDENTIAL`, and the review model and API base as
/// `IMPROVE_REVIEW_MODEL` and `IMPROVE_REVIEW_API_BASE`. It also exports the round's profile home as `CHARLIEBOT_HOME`,
/// where the review shim reads the key.
fn run_in_scope(world: &World, launch_id: &str, kind: &str) -> harness::Run {
  run_in_scope_with(world, launch_id, kind, &[])
}

/// [`run_in_scope`] with `extra` variables set over its environment, such as a `PATH` that names a stub.
fn run_in_scope_with(world: &World, launch_id: &str, kind: &str, extra: &[(&str, String)]) -> harness::Run {
  run_in_scope_for(world, launch_id, FOCUS_DIRECTION, kind, extra)
}

/// [`run_in_scope_with`] for an explicit direction, so a test that asserts on `/tmp/improve-<direction>-tmp` does not
/// race with the `FOCUS_DIRECTION` tests.
fn run_in_scope_for(
  world: &World,
  launch_id: &str,
  direction: &str,
  kind: &str,
  extra: &[(&str, String)],
) -> harness::Run {
  let launch = world.stage_launch(launch_id);
  fs::write(launch.join("args"), "").expect("the launch arguments file");
  fs::create_dir_all(format!("/tmp/improve-{direction}-tmp")).expect("the round's TMPDIR");
  fs::create_dir_all(world.remote_home.join("improve").join(direction)).expect("the round root");
  let bin = world.remote_home.join("improve/runner/bin");
  fs::create_dir_all(&bin).expect("the runner bin directory");
  harness::write_executable(&bin.join("improve-sandbox"), format!("#!/usr/bin/env bash\n{SCOPE_ENV_STUB}"));
  let mut env = harness::Env::new();
  env.insert("HOME".to_string(), world.remote_home.display().to_string());
  env.insert("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string());
  env.insert("FAKE_SCOPE_ENV".to_string(), world.sandbox.root().join("scope-env.txt").display().to_string());
  env.insert("FAKE_SCOPE_AUTH".to_string(), world.sandbox.root().join("scope-auth.json").display().to_string());
  env.insert("IMPROVE_REVIEW_CREDENTIAL".to_string(), REVIEW_CREDENTIAL.to_string());
  env.insert("IMPROVE_REVIEW_MODEL".to_string(), REVIEW_MODEL.to_string());
  env.insert("IMPROVE_REVIEW_API_BASE".to_string(), REVIEW_API_BASE.to_string());
  env.insert("CHARLIEBOT_HOME".to_string(), world.remote_home.join("improve/home").display().to_string());
  for (name, value) in extra {
    env.insert((*name).to_string(), value.clone());
  }
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("dev/improve/remote-runner.sh")).args([
    "--in-scope",
    launch_id,
    direction,
    kind,
  ]);
  harness::run(&mut command)
}

#[test]
#[ignore = "integration"]
fn the_in_scope_runner_exports_the_member_token_and_no_review_key() {
  let world = World::new();
  // The credentials file holds the review key, and the round's environment still gets none of it: the review's
  // charlie-code reads the key from that file when it starts.
  fs::write(
    world.remote_home.join("improve/home/credentials.yaml"),
    format!("{REVIEW_CREDENTIAL}:\n  api_key: {FOCUSED_REVIEW_KEY}\n"),
  )
  .expect("the fixture credentials file");
  let launch_id = "20261101T090000Z-1-300";
  let launch = world.stage_launch(launch_id);
  write_member_credential(&launch, &[("ANTHROPIC_AUTH_TOKEN", FOCUSED_MEMBER_TOKEN)]);

  let run = run_in_scope(&world, launch_id, "claude");

  assert_eq!(run.code, 0, "{}", run.describe());
  let text = fs::read_to_string(world.sandbox.root().join("scope-env.txt"))
    .expect("the stub improve-sandbox recorded its environment");
  assert!(text.contains(&format!("ANTHROPIC_AUTH_TOKEN_SHA256={}\n", digest(FOCUSED_MEMBER_TOKEN))), "{text}");
  assert!(text.contains("CHARLIE_CODE_API_KEY=unset\n"), "the round's environment holds the review key:\n{text}");
}

/// The TMPDIR that the stub improve-sandbox of the last [`run_in_scope`] call recorded.
fn scope_tmpdir(world: &World) -> PathBuf {
  let text = fs::read_to_string(world.sandbox.root().join("scope-env.txt")).expect("the stub recorded its environment");
  let tmpdir = text.lines().find_map(|line| line.strip_prefix("TMPDIR=")).expect("the stub recorded TMPDIR");
  PathBuf::from(tmpdir)
}

#[test]
#[ignore = "integration"]
fn a_scope_removes_the_directories_earlier_scopes_made_and_keeps_the_session_tmpdir() {
  let world = World::new();
  let session_tmpdir = PathBuf::from(format!("/tmp/improve-{SCOPE_TMPDIR_DIRECTION}-tmp"));
  let _ = fs::remove_dir_all(&session_tmpdir);
  fs::create_dir_all(session_tmpdir.join("session-copy")).expect("a directory that no runner made");
  fs::write(session_tmpdir.join("session-copy/data.txt"), "session data").expect("the session copy");
  fs::write(session_tmpdir.join("loose.txt"), "loose").expect("a loose file in the session TMPDIR");

  let first = run_in_scope_for(&world, "20261101T090000Z-1-310", SCOPE_TMPDIR_DIRECTION, "claude", &[]);
  assert_eq!(first.code, 0, "{}", first.describe());
  let first_tmpdir = scope_tmpdir(&world);
  assert_eq!(first_tmpdir.parent(), Some(session_tmpdir.as_path()), "{}", first_tmpdir.display());
  fs::write(first_tmpdir.join("killed-copy.bin"), "copy").expect("the first scope's copy");

  let second = run_in_scope_for(&world, "20261101T090000Z-1-311", SCOPE_TMPDIR_DIRECTION, "claude", &[]);
  let second_tmpdir = scope_tmpdir(&world);
  let first_exists = first_tmpdir.exists();
  let second_is_dir = second_tmpdir.is_dir();
  let session_copy =
    fs::read_to_string(session_tmpdir.join("session-copy/data.txt")).expect("the session copy survived");
  let loose = fs::read_to_string(session_tmpdir.join("loose.txt")).expect("the loose file survived");
  let _ = fs::remove_dir_all(&session_tmpdir);

  assert_eq!(second.code, 0, "{}", second.describe());
  assert!(!first_exists, "the earlier scope's directory survived: {}", first_tmpdir.display());
  assert!(second_is_dir, "the second scope made no TMPDIR: {}", second_tmpdir.display());
  assert_eq!(second_tmpdir.parent(), Some(session_tmpdir.as_path()), "{}", second_tmpdir.display());
  assert_eq!(session_copy, "session data");
  assert_eq!(loose, "loose");
}

/// Fake charlie-code for a review that outlives its agent: it records its process id, reports its start, and stays
/// until the test writes the release file or its wait bound passes.
const LEFT_REVIEW: &str = r#"# Fake charlie-code: record this process, report the start, and wait for the release.
printf '%s\n' "$$" >"$FAKE_REVIEW_PID"
: >"$FAKE_REVIEW_STARTED"
for _ in $(seq 1 600); do
  [[ -e $FAKE_REVIEW_RELEASE ]] && break
  sleep 0.1
done
"#;

/// Whether process `pid` has ended: its entry is gone, or it is a zombie that its parent has not reaped.
fn process_ended(pid: u32) -> bool {
  match fs::read_to_string(format!("/proc/{pid}/stat")) {
    Ok(stat) => stat.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
    Err(error) => panic!("cannot read the stat of process {pid}: {error}"),
  }
}

#[test]
#[ignore = "integration"]
fn a_review_that_outlives_its_agent_fails_the_round_and_names_it() {
  let world = World::new();
  let launch_id = "20261101T090200Z-1-302";
  let launch = world.remote_home.join("improve/launch").join(launch_id);
  fs::create_dir_all(&launch).expect("the launch directory");
  write_member_credential(&launch, &[("ANTHROPIC_AUTH_TOKEN", FOCUSED_MEMBER_TOKEN)]);
  fs::write(
    world.remote_home.join("improve/home/credentials.yaml"),
    format!("{REVIEW_CREDENTIAL}:\n  api_key: {FOCUSED_REVIEW_KEY}\n"),
  )
  .expect("the fixture credentials file");
  // The review starts through the installed shim, as a round's review does: the shim execs the host's charlie-code,
  // which is the stub below, so the review process is still the one the runner's tracking must find.
  let runner_bin = world.remote_home.join("improve/runner/bin");
  fs::create_dir_all(&runner_bin).expect("the runner bin directory");
  let shim = fs::read_to_string(harness::repo_root().join("dev/improve/review-charlie-code.sh")).expect("the shim");
  harness::write_executable(&runner_bin.join("charlie-code"), shim);
  harness::write_executable(
    &world.remote_home.join(".local/bin/charlie-code"),
    format!("#!/usr/bin/env bash\n{LEFT_REVIEW}"),
  );
  let pid_file = world.sandbox.root().join("review-pid");
  let started = world.sandbox.root().join("review-started");
  let release = world.sandbox.root().join("review-release");
  let path = format!("{}:/usr/local/bin:/usr/bin:/bin", runner_bin.display());

  let run = run_in_scope_with(
    &world,
    launch_id,
    "claude",
    &[
      ("PATH", path),
      ("FAKE_SCOPE_LEFT_REVIEW", "1".to_string()),
      ("FAKE_REVIEW_PID", pid_file.display().to_string()),
      ("FAKE_REVIEW_STARTED", started.display().to_string()),
      ("FAKE_REVIEW_RELEASE", release.display().to_string()),
    ],
  );

  // The stub ends before this test does, so no later test's scope holds it.
  fs::write(&release, "").expect("the release file");
  let pid: u32 = fs::read_to_string(&pid_file)
    .unwrap_or_else(|error| panic!("the stub review wrote no process id ({error}): {}", run.describe()))
    .trim()
    .parse()
    .expect("a process id");
  assert!(wait_until(Duration::from_secs(10), || process_ended(pid)), "the stub review {pid} did not end");
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("charlie-code"), "the failure does not name the review: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn an_unsupported_member_kind_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let mut arguments = world.agent_arguments();
  arguments[0] = "/opt/agents/gemini".to_string();
  let run = harness::run(&mut world.launcher_command_for(&world.env(), &arguments));
  assert_refused_before_input(&world, &run, "the member kind 'gemini' is not supported");
}

#[test]
#[ignore = "integration"]
fn a_gateway_member_without_its_token_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let mut env = world.env();
  env.remove("ANTHROPIC_AUTH_TOKEN");
  let run = world.run(&env);
  assert_refused_before_input(&world, &run, "ANTHROPIC_AUTH_TOKEN");
}

#[test]
#[ignore = "integration"]
fn a_missing_review_key_exits_7_before_any_input_is_sent() {
  let world = World::new();
  fs::write(world.profile_home.join("credentials.yaml"), "synthetic-decoy:\n  api_key: decoy-value\n")
    .expect("the profile home's credentials without the review section");
  let run = world.run(&world.env());
  assert_refused_before_input(&world, &run, REVIEW_CREDENTIAL);
}

#[test]
#[ignore = "integration"]
fn a_round_without_its_written_review_key_exits_7_before_inventory_or_scope() {
  let world = World::new();
  let launch_id = "20261101T090200Z-1-302";
  let launch = world.stage_launch(launch_id);
  harness::copy_file(
    &harness::repo_root().join("dev/improve/review-charlie-code.sh"),
    &launch.join("review-charlie-code.sh"),
  );
  fs::write(launch.join("env"), format!("IMPROVE_REVIEW_CREDENTIAL={REVIEW_CREDENTIAL}\0"))
    .expect("the launch environment");
  fs::write(world.remote_home.join("improve/home/credentials.yaml"), "synthetic-decoy:\n  api_key: decoy-value\n")
    .expect("the round credentials file without the review key");

  let cargo_log = world.sandbox.root().join("preflight-cargo-builds.log");
  let mut env = world.env();
  env.insert("HOME".to_string(), world.remote_home.display().to_string());
  env.insert("FAKE_CARGO_LOG".to_string(), cargo_log.display().to_string());
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("dev/improve/remote-runner.sh")).args([
    launch_id,
    DIRECTION,
    &world.commit,
    "claude",
    world::LOCAL_CLAUDE_VERSION,
  ]);
  let run = harness::run(&mut command);

  assert_refused_before_start(&world, &run, REVIEW_CREDENTIAL);
  assert!(!launch.join("inflight-files.txt").exists(), "the runner listed in-flight files before preflight");
  assert!(!cargo_log.exists(), "the runner built the CLI before preflight");
}

#[test]
#[ignore = "integration"]
fn a_subscription_claude_without_its_login_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let run = world.run(&world.host_login_env());
  assert_refused_before_input(&world, &run, &world.claude_login_file().display().to_string());
}

#[test]
#[ignore = "integration"]
fn a_codex_member_without_its_login_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let run = harness::run(&mut world.launcher_command_for(&world.host_login_env(), &world.codex_arguments()));
  assert_refused_before_input(&world, &run, &world.local_home.join(".codex/auth.json").display().to_string());
}

#[test]
#[ignore = "integration"]
fn a_profile_home_on_the_host_exits_7_and_starts_no_scope() {
  let world = World::new();
  fs::create_dir_all(world.remote_home.join(".charliebot")).expect("a profile home on the host");
  let run = world.run(&world.env());
  assert_refused_before_start(&world, &run, ".charliebot exists");
}

#[test]
#[ignore = "integration"]
fn a_refused_write_boundary_exits_7_and_starts_no_scope() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_SANDBOX_REFUSE".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_refused_before_start(&world, &run, "the write boundary does not start");
}

/// The assertion that the launcher refused a launch with exit 7 before it sent any input, and that stderr
/// names `needle`.
fn assert_refused_before_input(world: &World, run: &harness::Run, needle: &str) {
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(stderr(run).contains(needle), "the refusal does not name {needle}: {}", run.describe());
  assert!(world.ssh_arguments().is_empty(), "the refused launch sent input:\n{}", world.ssh_arguments());
}

/// The assertion that the runner refused a round with exit 7, that stderr names `needle`, and that the round
/// started no scope and no agent.
fn assert_refused_before_start(world: &World, run: &harness::Run, needle: &str) {
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(stderr(run).contains(needle), "the refusal does not name {needle}: {}", run.describe());
  assert!(!world.scope_started(), "the refused round started a scope:\n{}", world.systemctl_calls());
  assert!(!world.claude_ran(), "the refused round started claude");
  assert!(!world.codex_ran(), "the refused round started codex");
}

const FOCUSED_REVIEW_KEY: &str = "focused-review-key";
const FOCUSED_MEMBER_TOKEN: &str = "focused-member-token";

#[test]
#[ignore = "integration"]
fn the_round_writes_only_the_evidence_copy_on_this_host() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());

  let evidence_root = world.local_home.join("improve");
  let tree = world.tree(&evidence_root);
  assert!(!tree.is_empty(), "the launcher copied no evidence back:\n{}", run.describe());
  for path in &tree {
    assert!(
      path == "perf" || path == "perf/evidence" || path.starts_with("perf/evidence/"),
      "the launcher wrote outside {}/{DIRECTION}/evidence: {path}",
      evidence_root.display()
    );
  }
  let copied = world.local_evidence().join("claude-argv.txt");
  assert!(copied.is_file(), "the round's evidence did not arrive: {:?}", world.tree(&evidence_root));
}

#[test]
#[ignore = "integration"]
fn a_failed_evidence_copy_reports_the_copys_status_and_the_round_still_starts() {
  let world = World::new();
  let mut env = world.env();
  env.insert("SSH_FAIL_EVIDENCE_LISTING".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the failed copy stopped the round itself:\n{}", run.describe());
  let message = stderr(&run);
  assert!(
    message.contains("the evidence copy stopped (status 7)"),
    "the copy's own status did not reach stderr: {message}"
  );
}

#[test]
#[ignore = "integration"]
fn a_second_evidence_copy_leaves_complete_evidence_alone() {
  let world = World::new();
  let previous = world.remote_home.join("improve").join(DIRECTION).join("evidence").join("20261101T090000Z");
  fs::create_dir_all(&previous).expect("the previous round's evidence directory");
  fs::write(previous.join("measure.txt"), "measurement\n").expect("the previous round's evidence");

  let first = world.run(&world.env());

  assert_eq!(first.code, 0, "{}", first.describe());
  let copied = world.local_evidence().join("20261101T090000Z/measure.txt");
  assert_eq!(fs::read_to_string(&copied).expect("the evidence arrived"), "measurement\n");
  // A second copy that transferred this file again would rewrite it, which moves its status change
  // time. The wait keeps the two times in different seconds even on a coarse filesystem.
  std::thread::sleep(Duration::from_secs(1));
  let before = status_change_time(&copied);

  let second = world.run(&world.env());

  assert_eq!(second.code, 0, "{}", second.describe());
  assert_eq!(
    status_change_time(&copied),
    before,
    "the second copy rewrote evidence that was already complete:\n{}",
    second.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_partly_copied_evidence_directory_completes_on_the_next_copy() {
  let world = World::new();
  let remote_round = world.remote_home.join("improve").join(DIRECTION).join("evidence").join("20261101T090000Z");
  fs::create_dir_all(&remote_round).expect("the previous round's evidence directory");
  for (name, contents) in
    [("complete.txt", "complete\n"), ("partial.txt", "partial evidence\n"), ("missing.txt", "missing\n")]
  {
    fs::write(remote_round.join(name), contents).expect("the previous round's evidence");
  }
  // The state a stopped copy leaves: one file complete, one truncated, one absent.
  let local_round = world.local_evidence().join("20261101T090000Z");
  fs::create_dir_all(&local_round).expect("the partly copied directory");
  fs::write(local_round.join("complete.txt"), "complete\n").expect("the complete file");
  fs::write(local_round.join("partial.txt"), "part").expect("the truncated file");

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  for (name, contents) in
    [("complete.txt", "complete\n"), ("partial.txt", "partial evidence\n"), ("missing.txt", "missing\n")]
  {
    assert_eq!(
      fs::read_to_string(local_round.join(name)).unwrap_or_else(|error| panic!("{name}: {error}")),
      contents,
      "the next copy did not complete {name}:\n{}",
      run.describe()
    );
  }
}

/// The time a file's status last changed, as `(seconds, nanoseconds)`.
fn status_change_time(path: &std::path::Path) -> (i64, i64) {
  let metadata = fs::metadata(path).expect("the local evidence's metadata");
  (metadata.ctime(), metadata.ctime_nsec())
}

#[test]
#[ignore = "integration"]
fn no_key_value_reaches_an_ssh_argument_or_any_file_but_the_credentials_file() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let arguments = world.ssh_arguments();
  assert!(!arguments.is_empty(), "the fake ssh recorded no call");
  let agent_arguments = world.claude_arguments_received().join("\n");
  for key in [GATEWAY_TOKEN, REVIEW_KEY, world::RUN_TOKEN, world::LOCAL_CLAUDE_OAUTH_TOKEN] {
    assert!(!arguments.contains(key), "a key value reached an ssh argument");
    assert!(!agent_arguments.contains(key), "a key value reached the agent's arguments");
  }
  assert_eq!(
    world.claude_environment().get("CLAUDE_CODE_OAUTH_TOKEN_SHA256").map(String::as_str),
    Some("unset"),
    "the launcher's own Claude login reached the round"
  );

  // The credentials file holds the review key only. The gateway token lived in the launch's member-credential file,
  // which the round removed when it ended.
  let credentials = world.remote_home.join("improve/home/credentials.yaml");
  let text = fs::read_to_string(&credentials).expect("the runner wrote the credentials file");
  assert!(text.contains(REVIEW_KEY) && !text.contains(GATEWAY_TOKEN), "the credentials file holds the wrong keys");
  let mode = fs::metadata(&credentials).expect("credentials metadata").permissions().mode() & 0o777;
  assert_eq!(mode, 0o600, "{credentials:?} has mode {mode:o}");
  assert!(files_holding(&world, &world.local_home, GATEWAY_TOKEN).is_empty(), "a key reached this host's files");
  assert!(files_holding(&world, &world.local_home, REVIEW_KEY).is_empty(), "a key reached this host's files");
  assert_eq!(
    files_holding(&world, &world.remote_home, REVIEW_KEY),
    ["improve/home/credentials.yaml"],
    "the review key reached a file other than the credentials file"
  );
  assert!(files_holding(&world, &world.remote_home, GATEWAY_TOKEN).is_empty(), "the gateway token outlived the round");
  for key in [world::RUN_TOKEN, world::LOCAL_CLAUDE_OAUTH_TOKEN] {
    assert!(files_holding(&world, &world.local_home, key).is_empty(), "a key reached this host's files");
    assert!(files_holding(&world, &world.remote_home, key).is_empty(), "a key reached the remote host's files");
  }
}

/// The assertion that the launcher refused a launch with exit 8 before it sent any input: the login's token has less
/// life than a round needs, and stderr names the login and says no round started.
fn assert_short_before_input(world: &World, run: &harness::Run, login: &Path) {
  assert_eq!(run.code, 8, "{}", run.describe());
  let message = stderr(run);
  assert!(
    message.contains(&login.display().to_string()) && message.contains("min of access-token life left"),
    "the refusal does not name the login and its life: {message}"
  );
  assert!(!message.contains("401") && !message.contains("403"), "the refusal names an auth status: {message}");
  assert!(world.ssh_arguments().is_empty(), "the short login sent input:\n{}", world.ssh_arguments());
}

#[test]
#[ignore = "integration"]
fn a_subscription_login_that_does_not_parse_exits_7_naming_the_file_before_any_input() {
  let world = World::new();
  fs::create_dir_all(&world.claude_login_dir).expect("the Claude login directory");
  fs::write(world.claude_login_file(), "{\"claudeAiOauth\":").expect("the truncated login");
  let run = world.run(&world.host_login_env());
  assert_refused_before_input(&world, &run, &world.claude_login_file().display().to_string());
}

#[test]
#[ignore = "integration"]
fn a_subscription_login_without_a_numeric_expiry_exits_7_naming_the_file_before_any_input() {
  let world = World::new();
  fs::create_dir_all(&world.claude_login_dir).expect("the Claude login directory");
  fs::write(world.claude_login_file(), r#"{"claudeAiOauth":{"accessToken":"synthetic","expiresAt":"soon"}}"#)
    .expect("the login without a numeric expiry");
  let run = world.run(&world.host_login_env());
  assert_refused_before_input(&world, &run, &world.claude_login_file().display().to_string());
}

#[test]
#[ignore = "integration"]
fn a_subscription_login_with_less_than_four_hours_of_token_life_exits_8_before_any_input() {
  let world = World::new();
  world.write_claude_login(expiry_ms(3));
  let run = world.run(&world.host_login_env());
  assert_short_before_input(&world, &run, &world.claude_login_file());
  assert!(!world.claude_ran(), "a short login started claude:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_codex_login_with_less_than_four_hours_of_token_life_exits_8_before_any_input() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 2 * 3600);
  let run = harness::run(&mut world.launcher_command_for(&world.host_login_env(), &world.codex_arguments()));
  assert_short_before_input(&world, &run, &world.local_home.join(".codex/auth.json"));
  assert!(!world.codex_ran(), "a short login started codex:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_missing_token_life_threshold_exits_7_before_any_input() {
  let world = World::new();
  let mut env = world.env();
  env.remove("IMPROVE_TOKEN_MIN_SECONDS");

  let run = world.run(&env);

  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(stderr(&run).contains("IMPROVE_TOKEN_MIN_SECONDS is not set"), "{}", run.describe());
  assert!(world.ssh_arguments().is_empty(), "a missing threshold contacted the host");
}

/// The forwarded form of a codex login whose access token expires at `exp`: the refresh token is the empty string,
/// as the launch sends it.
fn forwarded_codex_login(exp: i64) -> String {
  let mut login: Value = serde_json::from_str(&world::codex_login_json(exp)).expect("the fixture login is JSON");
  login["tokens"]["refresh_token"] = json!("");
  login.to_string()
}

/// Make a codex launch's directory with its member credential holding `forwarded`, the login the launch carries.
fn codex_in_scope_fixture(world: &World, launch_id: &str, forwarded: &str) {
  let launch = world.stage_launch(launch_id);
  write_member_credential(&launch, &[("IMPROVE_CODEX_AUTH", forwarded)]);
  fs::write(
    world.remote_home.join("improve/home/credentials.yaml"),
    format!("{REVIEW_CREDENTIAL}:\n  api_key: {REVIEW_KEY}\n"),
  )
  .expect("the round's credentials file");
}

#[test]
#[ignore = "integration"]
fn the_in_scope_codex_round_runs_under_a_codex_home_holding_its_login_at_mode_600() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  let launch_id = "20261101T090300Z-1-303";
  let forwarded = forwarded_codex_login(world::now_seconds() + 200 * 3600);
  codex_in_scope_fixture(&world, launch_id, &forwarded);
  fs::create_dir(&round_tmp).expect("the round's directory");

  let run = run_in_scope(&world, launch_id, "codex");

  let scope_env = fs::read_to_string(world.sandbox.root().join("scope-env.txt")).unwrap_or_default();
  let found = fs::read_to_string(world.sandbox.root().join("scope-auth.json")).ok();
  let _ = fs::remove_dir_all(&round_tmp);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(scope_env.contains(&format!("CODEX_HOME={}\n", round_tmp.join("codex-home").display())), "{scope_env}");
  assert!(scope_env.contains("CODEX_AUTH_MODE=600\n"), "the login is not mode 600:\n{scope_env}");
  let found = found.expect("the stub improve-sandbox copied the login");
  assert_eq!(
    serde_json::from_str::<Value>(&found).expect("the round's login is JSON"),
    serde_json::from_str::<Value>(&forwarded).expect("the forwarded login is JSON"),
    "the round's login is not the forwarded one"
  );
}

#[test]
#[ignore = "integration"]
fn a_codex_round_refuses_a_round_directory_that_is_a_symbolic_link() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  let target = world.sandbox.dir("elsewhere");
  std::os::unix::fs::symlink(&target, &round_tmp).expect("the symbolic link");
  let launch_id = "20261101T090400Z-1-304";
  codex_in_scope_fixture(&world, launch_id, &forwarded_codex_login(world::now_seconds() + 200 * 3600));

  let run = run_in_scope(&world, launch_id, "codex");

  let _ = fs::remove_file(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains(&format!("{} is a symbolic link", round_tmp.display())), "{}", run.describe());
  assert!(!target.join("codex-home").exists(), "a Codex home was made through the link");
}

#[test]
#[ignore = "integration"]
fn a_codex_round_refuses_a_codex_home_that_is_a_symbolic_link() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  fs::create_dir(&round_tmp).expect("the round's directory");
  let target = world.sandbox.dir("elsewhere");
  let codex_home = round_tmp.join("codex-home");
  std::os::unix::fs::symlink(&target, &codex_home).expect("the symbolic link");
  let launch_id = "20261101T090500Z-1-305";
  codex_in_scope_fixture(&world, launch_id, &forwarded_codex_login(world::now_seconds() + 200 * 3600));

  let run = run_in_scope(&world, launch_id, "codex");

  let _ = fs::remove_dir_all(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains(&format!("{} is a symbolic link", codex_home.display())), "{}", run.describe());
  assert!(!target.join("auth.json").exists(), "a login was written through the link");
}

#[test]
#[ignore = "integration"]
fn a_codex_round_refuses_a_codex_home_another_user_owns() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  fs::create_dir_all(round_tmp.join("codex-home")).expect("the Codex home");
  // A stat that reports another owner for the Codex home only; the round's own directory keeps its real owner.
  let stub = world.sandbox.dir("owner-stub");
  harness::write_executable(
    &stub.join("stat"),
    r#"#!/usr/bin/env bash
# Reports uid 0 for the Codex home, and the real owner for every other path.
if [[ $* == *codex-home* ]]; then
  printf '0\n'
else
  command -p stat "$@"
fi
"#,
  );
  let launch_id = "20261101T090600Z-1-306";
  codex_in_scope_fixture(&world, launch_id, &forwarded_codex_login(world::now_seconds() + 200 * 3600));
  let path = format!("{}:/usr/local/bin:/usr/bin:/bin", stub.display());

  let run = run_in_scope_with(&world, launch_id, "codex", &[("PATH", path)]);

  let _ = fs::remove_dir_all(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("codex-home is not owned by the runner's user"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_codex_round_refuses_a_parent_directory_another_user_owns() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  fs::create_dir(&round_tmp).expect("the round's directory");
  let stub = world.sandbox.dir("owner-stub");
  harness::write_executable(
    &stub.join("stat"),
    format!(
      r#"#!/usr/bin/env bash
# Reports uid 0 for the round directory, and the real owner for every other path.
if [[ $* == *"{}"* ]]; then
  printf '0\n'
else
  command -p stat "$@"
fi
"#,
      round_tmp.display()
    ),
  );
  let launch_id = "20261101T090700Z-1-307";
  codex_in_scope_fixture(&world, launch_id, &forwarded_codex_login(world::now_seconds() + 200 * 3600));
  let path = format!("{}:/usr/local/bin:/usr/bin:/bin", stub.display());

  let run = run_in_scope_with(&world, launch_id, "codex", &[("PATH", path)]);

  let _ = fs::remove_dir_all(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("is not owned by the runner's user"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_start_refuses_a_codex_parent_symlink_before_removing_its_target_login() {
  let world = World::new_with_direction(SYMLINK_DIRECTION);
  let round_tmp = PathBuf::from(format!("/tmp/improve-{SYMLINK_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  let target = world.sandbox.dir("elsewhere");
  let auth = target.join("codex-home/auth.json");
  fs::create_dir_all(auth.parent().expect("the target Codex home")).expect("the target Codex home");
  fs::write(&auth, "synthetic-codex-auth").expect("the target login");
  std::os::unix::fs::symlink(&target, &round_tmp).expect("the round directory symlink");

  let run = world.run(&world.env());

  let login_kept = fs::read_to_string(&auth).expect("the target login remains readable");
  let _ = fs::remove_file(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("is a symbolic link"), "{}", run.describe());
  assert!(login_kept == "synthetic-codex-auth", "the start sweep changed the target login");
}

#[test]
#[ignore = "integration"]
fn a_start_refuses_a_codex_home_symlink_before_removing_its_target_login() {
  let world = World::new_with_direction(SYMLINK_DIRECTION);
  let round_tmp = PathBuf::from(format!("/tmp/improve-{SYMLINK_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  fs::create_dir(&round_tmp).expect("the round directory");
  let target = world.sandbox.dir("elsewhere");
  let auth = target.join("auth.json");
  fs::write(&auth, "synthetic-codex-auth").expect("the target login");
  let codex_home = round_tmp.join("codex-home");
  std::os::unix::fs::symlink(&target, &codex_home).expect("the Codex home symlink");

  let run = world.run(&world.env());

  let login_kept = fs::read_to_string(&auth).expect("the target login remains readable");
  let _ = fs::remove_file(&codex_home);
  let _ = fs::remove_dir(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("is a symbolic link"), "{}", run.describe());
  assert!(login_kept == "synthetic-codex-auth", "the start sweep changed the target login");
}

#[test]
#[ignore = "integration"]
fn a_duplicate_codex_fire_leaves_the_live_login_and_removes_its_own_launch_directory() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut leader = spawn_live_round(&world, DIRECTION);
  let live_login = world.codex_home_directory().join("auth.json");
  fs::create_dir_all(live_login.parent().expect("the Codex home")).expect("the live round's Codex home");
  fs::write(&live_login, "LIVE-CODEX-LOGIN").expect("the live round's login");
  let mut env = world.host_login_env();
  env.insert("FAKE_SCOPE_STATE".to_string(), "active".to_string());

  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));

  stop_live_round(&mut leader);
  let kept = fs::read_to_string(&live_login);
  let _ = fs::remove_file(&live_login);
  assert_eq!(run.code, 5, "{}", run.describe());
  assert!(kept.as_ref().is_ok_and(|login| login == "LIVE-CODEX-LOGIN"), "the duplicate fire changed the live login");
  assert!(!world.codex_ran(), "the duplicate fire started codex:\n{}", run.describe());
  let launches: Vec<String> = fs::read_dir(world.remote_home.join("improve/launch"))
    .expect("the launch directories")
    .map(|entry| entry.expect("a launch directory entry").file_name().to_string_lossy().into_owned())
    .collect();
  assert_eq!(launches, ["20261103T120000Z-9-900"], "the duplicate fire left its launch directory");
}

#[test]
#[ignore = "integration"]
fn a_start_removes_a_dead_launchs_member_credential_and_keeps_a_live_launchs() {
  let world = World::new();
  let dead = world.remote_home.join("improve/launch/20261001T000000Z-7-70");
  fs::create_dir_all(&dead).expect("the dead launch's directory");
  write_member_credential(&dead, &[("ANTHROPIC_AUTH_TOKEN", "synthetic-dead-launch-token")]);
  let mut leader = spawn_live_round(&world, "ux");
  let live = world.remote_home.join("improve/launch/20261103T120000Z-9-900/member-credential");
  write_member_credential(
    live.parent().expect("the live launch's directory"),
    &[("ANTHROPIC_AUTH_TOKEN", "synthetic-live-launch-token")],
  );

  let run = world.run(&world.env());

  let live_kept = live.exists();
  stop_live_round(&mut leader);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(!dead.join("member-credential").exists(), "a start kept a dead launch's member credential");
  assert!(live_kept, "a start removed a live launch's member credential");
}

#[test]
#[ignore = "integration"]
fn a_prepared_launch_keeps_its_member_credential_until_its_wrapper_starts() {
  let world = World::new();
  let ready = world.sandbox.root().join("main-run-ready");
  let release = world.sandbox.root().join("main-run-release");
  let mut first_env = world.env();
  first_env.insert("FAKE_SSH_PAUSE_MAIN_RUN".to_string(), "1".to_string());
  first_env.insert("FAKE_SSH_MAIN_RUN_READY".to_string(), ready.display().to_string());
  first_env.insert("FAKE_SSH_MAIN_RUN_RELEASE".to_string(), release.display().to_string());
  let mut first_command = world.launcher_command(&first_env);
  let first = harness::spawn(&mut first_command);

  let reached_gap = wait_until(Duration::from_secs(15), || ready.exists());
  if !reached_gap {
    let _ = fs::write(&release, "continue\n");
    let _ = first.wait_with_output();
    panic!("the first launch did not pause after preparing its member credential");
  }
  let first_launch = world.launch_directory().expect("the prepared launch directory");
  let member_credential = first_launch.join("member-credential");
  let second = world.run(&world.env());
  let survived_sweep = member_credential.is_file();
  fs::write(&release, "continue\n").expect("release the first launch");
  let first_result = first.wait_with_output().expect("the first launch finished");

  assert_eq!(second.code, 0, "{}", second.describe());
  assert!(survived_sweep, "a concurrent start removed the prepared launch's member credential");
  assert!(first_result.status.success(), "the prepared launch failed before its wrapper started");
  assert!(!first_launch.exists(), "the prepared launch directory was not cleaned up");
}

#[test]
#[ignore = "integration"]
fn a_start_removes_an_idle_directions_codex_login_and_keeps_a_live_rounds() {
  let world = World::new();
  let login = world.codex_home_directory().join("auth.json");
  fs::create_dir_all(world.codex_home_directory()).expect("the Codex home");
  fs::write(&login, "IDLE-CODEX-LOGIN").expect("the idle login");

  let idle_run = world.run(&world.env());

  assert_eq!(idle_run.code, 0, "{}", idle_run.describe());
  assert!(!login.exists(), "a start kept an idle direction's Codex login");

  let mut leader = spawn_live_round(&world, DIRECTION);
  fs::write(&login, "LIVE-CODEX-LOGIN").expect("the live round's login");
  let live_run = world.run(&world.env());
  let kept = fs::read_to_string(&login);
  stop_live_round(&mut leader);
  let _ = fs::remove_file(&login);

  // The clone sync refuses a live round of the direction before the sweep, so the second start exits 5 and the live
  // login stays. The start refuses the live round's leader with that exit code, as the runner's header says.
  assert_eq!(live_run.code, 5, "{}", live_run.describe());
  assert!(
    stderr(&live_run).contains("the previous round still runs"),
    "a start did not refuse the live round:\n{}",
    live_run.describe()
  );
  assert!(kept.as_ref().is_ok_and(|login| login == "LIVE-CODEX-LOGIN"), "a start removed a live round's login");
}

#[test]
#[ignore = "integration"]
fn two_launches_prepared_at_once_keep_their_own_member_records_and_the_review_key() {
  let world = World::new();
  let first = "20261101T100000Z-1-400";
  let second = "20261101T100000Z-2-401";
  let launches = world.remote_home.join("improve/launch");
  fs::create_dir_all(launches.join(first)).expect("the first launch directory");
  fs::create_dir_all(launches.join(second)).expect("the second launch directory");
  let review = [
    ("CHARLIE_CODE_API_KEY", REVIEW_KEY),
    ("IMPROVE_REVIEW_CREDENTIAL", REVIEW_CREDENTIAL),
    ("IMPROVE_REVIEW_MODEL", REVIEW_MODEL),
    ("IMPROVE_REVIEW_API_BASE", REVIEW_API_BASE),
  ];
  let first_member = ("ANTHROPIC_AUTH_TOKEN", "synthetic-first-gateway-token");
  let second_member = ("IMPROVE_CODEX_AUTH", "{\"synthetic\":\"second-launch\"}");
  let children: Vec<_> = [(first, first_member), (second, second_member)]
    .into_iter()
    .map(|(launch_id, member)| {
      let mut records = review.to_vec();
      records.push(member);
      prepare_in_background(&world, launch_id, &records)
    })
    .collect();
  for child in children {
    let output = child.wait_with_output().expect("the prepare call finished");
    assert!(output.status.success(), "prepare failed: {}", String::from_utf8_lossy(&output.stderr));
  }

  assert!(
    member_records(&launches.join(first).join("member-credential"))
      == ["ANTHROPIC_AUTH_TOKEN=synthetic-first-gateway-token"],
    "the first launch's member record changed"
  );
  assert!(
    member_records(&launches.join(second).join("member-credential"))
      == ["IMPROVE_CODEX_AUTH={\"synthetic\":\"second-launch\"}"],
    "the second launch's member record changed"
  );
  let credentials =
    fs::read_to_string(world.remote_home.join("improve/home/credentials.yaml")).expect("the review credentials file");
  assert!(
    credentials == format!("{REVIEW_CREDENTIAL}:\n  api_key: {REVIEW_KEY}\n"),
    "the review credentials file has the wrong content"
  );
  for launch_id in [first, second] {
    let mode = fs::metadata(launches.join(launch_id).join("member-credential"))
      .expect("the member credential")
      .permissions()
      .mode();
    assert_eq!(mode & 0o777, 0o600, "the member credential of {launch_id} has mode {mode:o}");
  }
}

/// Start `--prepare` for one launch with its records on standard input, and return the running child.
fn prepare_in_background(world: &World, launch_id: &str, records: &[(&str, &str)]) -> std::process::Child {
  let mut env = harness::Env::new();
  env.insert("HOME".to_string(), world.remote_home.display().to_string());
  env.insert("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string());
  let mut command = harness::command("bash", &env);
  command
    .arg(harness::repo_root().join("dev/improve/remote-runner.sh"))
    .args(["--prepare", launch_id])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  let mut child = command.spawn().expect("the prepare call starts");
  let mut bytes = Vec::new();
  for (name, value) in records {
    bytes.extend_from_slice(format!("{name}={value}").as_bytes());
    bytes.push(0);
  }
  if let Some(mut stdin) = child.stdin.take() {
    stdin.write_all(&bytes).expect("the records reach the prepare call");
  }
  child
}

/// The records of a member-credential file, each as its `NAME=value` text.
fn member_records(path: &Path) -> Vec<String> {
  let bytes = fs::read(path).expect("the member credential file");
  bytes
    .split(|byte| *byte == 0)
    .filter(|record| !record.is_empty())
    .map(|record| String::from_utf8_lossy(record).into_owned())
    .collect()
}

#[test]
#[ignore = "integration"]
fn claudes_exit_code_passes_through() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_EXIT".to_string(), "7".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(world.claude_ran());
}

#[test]
#[ignore = "integration"]
fn a_round_gh_write_with_a_credential_value_stops_the_round_with_its_kind() {
  // The round's member runs one gh write whose text carries the member's own login: the shim refuses it before the
  // real gh runs, and the round ends with the credential reason.
  let world = World::new();
  let mut env = world.env();
  env.insert(
    "FAKE_CLAUDE_COMMAND".to_string(),
    r#"gh pr comment 1 --body "leaked $CLAUDE_CODE_OAUTH_TOKEN $ANTHROPIC_AUTH_TOKEN""#.to_string(),
  );
  let run = world.run(&env);
  assert_eq!(run.code, 3, "{}", run.describe());
  let text = format!("{}{}", run.stdout, run.stderr);
  assert!(text.contains("environment variable"), "the kind is the reason: {text}");
  assert!(!text.contains(world::LOGIN_TOKEN) && !text.contains(world::GATEWAY_TOKEN), "no value is printed: {text}");
}

#[test]
#[ignore = "integration"]
fn the_runner_generates_a_missing_shared_mock_home_before_the_round_starts() {
  let world = World::new();
  let mock_home = world.remote_home.join("improve/mock-home").join(world::MOCK_HOME_VERSION);
  assert!(!mock_home.exists(), "the fixture starts with a shared mock home");

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(
    fs::read_to_string(&world.mock_home_log).expect("the mock-home calls"),
    format!("version\ngenerate {}\nround perf\n", mock_home.display()),
    "the runner did not generate the complete mock home before starting the round"
  );
  assert_eq!(
    fs::read_to_string(mock_home.join("manifest.json")).expect("the generated manifest"),
    "synthetic complete mock home\n"
  );
}

#[test]
#[ignore = "integration"]
fn a_mock_home_generation_failure_names_the_cause_and_stops_the_round() {
  let world = World::new();
  let mock_home = world.remote_home.join("improve/mock-home").join(world::MOCK_HOME_VERSION);
  let mut env = world.env();
  env.insert("FAKE_MOCK_HOME_FAIL".to_string(), "1".to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("synthetic mock-home generation failure: disk full"), "{}", run.describe());
  assert!(stderr(&run).contains(&mock_home.display().to_string()), "{}", run.describe());
  assert!(!world.claude_ran(), "the round started after mock-home generation failed");
  assert!(
    fs::read_to_string(&world.systemd_run_log).unwrap_or_default().is_empty(),
    "the runner started a scope after mock-home generation failed"
  );
  assert!(!mock_home.exists(), "the failed generation published a shared mock home");
}

#[test]
#[ignore = "integration"]
fn the_forwarded_variables_reach_the_round_and_the_run_token_and_this_hosts_config_dir_do_not() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());

  let text = fs::read_to_string(&world.claude_env).expect("the fake claude recorded its environment");
  let env = world.claude_environment();
  let value = |name: &str| env.get(name).map(String::as_str);
  let remote_home = world.remote_home.display().to_string();

  assert_eq!(value("ANTHROPIC_BASE_URL"), Some("https://gateway.example"));
  assert_eq!(value("ANTHROPIC_MODEL"), Some("synthetic-model"));
  assert!(text.contains("ANTHROPIC_CUSTOM_HEADERS=x-tag: synthetic\nx-line: two\n"), "{text}");
  assert_eq!(value("CLAUDE_CODE_EXTRA_BODY"), Some("{\"cache\":true}"));
  assert_eq!(value("BASH_DEFAULT_TIMEOUT_MS"), Some("900000"));
  assert_eq!(value("BASH_MAX_TIMEOUT_MS"), Some("900000"));
  assert_eq!(value("CHARLIEBOT_SESSION_ID"), Some(world::SESSION_ID));
  let round_home = format!("{remote_home}/improve/home");
  assert_eq!(value("CHARLIEBOT_HOME"), Some(round_home.as_str()));
  let config_dir = format!("{remote_home}/.claude-improve");
  assert_eq!(value("CLAUDE_CONFIG_DIR"), Some(config_dir.as_str()));
  let tmpdir = value("TMPDIR").expect("the round's TMPDIR");
  assert!(tmpdir.starts_with("/tmp/improve-perf-tmp/"), "the round's TMPDIR is not under the session TMPDIR: {tmpdir}");
  assert!(!text.contains("CHARLIEBOT_RUN_TOKEN"), "the run token reached the round:\n{text}");
  assert!(!text.contains(world::REMOTE_HOST), "a launcher setting reached the round:\n{text}");
}

#[test]
#[ignore = "integration"]
fn an_evidence_copy_past_its_limit_stops_and_the_round_still_exits_with_claudes_code() {
  let world = World::new();
  let mut env = world.env();
  env.insert("SSH_HANG_TAR".to_string(), "1".to_string());
  env.insert("IMPROVE_EVIDENCE_COPY_SECONDS".to_string(), "1".to_string());
  let start = Instant::now();
  let run = world.run(&env);
  let elapsed = start.elapsed();
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(elapsed < Duration::from_secs(20), "the launcher waited {elapsed:?} on the copy");
  assert!(world.claude_ran(), "the copy limit stopped the round itself:\n{}", run.describe());
  assert!(
    stderr(&run).contains("the evidence copy stopped (status 124)"),
    "the stopped copy did not report its timeout:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn an_approval_prompt_exits_3_inside_five_seconds() {
  let world = World::new();
  let approval = world.sandbox.root().join("approval-needed");
  fs::write(&approval, "1\n").expect("the approval marker");
  let mut env = world.env();
  env.insert("SSH_APPROVAL_FILE".to_string(), approval.display().to_string());
  let start = Instant::now();
  let run = world.run(&env);
  let elapsed = start.elapsed();
  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(elapsed < Duration::from_secs(5), "the launcher waited {elapsed:?} for the approval");
  assert!(stderr(&run).contains("/host-auth"), "the message names no approval page: {}", run.describe());
  assert!(!world.claude_ran(), "the round started although the connection needed an approval");
  assert!(
    !world.ssh_arguments().contains("remote-runner.sh"),
    "the launcher started the runner before the preflight passed:\n{}",
    world.ssh_arguments()
  );
}

#[test]
#[ignore = "integration"]
fn the_direction_comes_from_the_worker_session_and_its_task_file() {
  let world = World::new();
  // A task file of another session comes first in the directory. Its direction must not win.
  fs::write(
    world.sandbox.dir("profile/config.d/cron.d").join("improve-ux.yaml"),
    "prompt_file: dev/improve/ux.md\nsession_id: 00000000-0000-0000-0000-000000000000\n",
  )
  .expect("another task file");
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(scope.contains("--unit improve-perf"), "the round ran in another direction's scope: {scope}");
  assert!(!scope.contains("--unit improve-ux"), "{scope}");
}

#[test]
#[ignore = "integration"]
fn a_round_without_a_direction_exits_6() {
  let world = World::new();
  fs::remove_file(world.profile_home.join("config.d/cron.d").join(format!("improve-{DIRECTION}.yaml")))
    .expect("remove the task file");
  let run = world.run(&world.env());
  assert_eq!(run.code, 6, "{}", run.describe());
  assert!(!world.claude_ran(), "the round started without a direction");
  assert!(!world.ssh_arguments().contains("remote-runner.sh"), "{}", world.ssh_arguments());
}

#[test]
#[ignore = "integration"]
fn a_worker_session_without_a_parent_exits_6() {
  let world = World::new();
  let metadata = world.profile_home.join("sessions").join(world::SESSION_ID).join("metadata.json");
  fs::write(&metadata, "{\n  \"id\": \"synthetic\"\n}\n").expect("the metadata without a parent");
  let run = world.run(&world.env());
  assert_eq!(run.code, 6, "{}", run.describe());
  assert!(!world.claude_ran());
  assert!(!world.ssh_arguments().contains("remote-runner.sh"), "{}", world.ssh_arguments());
}

#[test]
#[ignore = "integration"]
fn an_existing_scope_exits_5_and_starts_no_claude() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_SYSTEMD_RUN_EXISTING".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 5, "{}", run.describe());
  assert!(!world.claude_ran(), "a round started inside the previous round's scope");
  assert!(stderr(&run).contains("previous round"), "{}", run.describe());
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(scope.contains("--unit improve-perf"), "{scope}");
}

#[test]
#[ignore = "integration"]
fn a_failed_scope_is_reset_and_the_round_starts() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_SCOPE_STATE".to_string(), "failed".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start after the reset:\n{}", run.describe());
  let calls: Vec<String> = world.systemctl_calls().lines().map(str::to_string).collect();
  let failed = calls.iter().position(|call| call.contains(&format!("is-failed --quiet improve-{DIRECTION}.scope")));
  let reset = calls.iter().position(|call| call.contains(&format!("reset-failed improve-{DIRECTION}.scope")));
  let (failed, reset) = (failed.expect("no is-failed call"), reset.expect("no reset-failed call"));
  assert!(failed < reset, "the reset did not follow the failed report:\n{calls:?}");
  assert!(stderr(&run).contains(&format!("cleared the failed scope improve-{DIRECTION}.scope")), "{}", run.describe());
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(scope.contains("--unit improve-perf"), "the round started no scope after the reset: {scope}");
}

#[test]
#[ignore = "integration"]
fn an_active_scope_whose_round_is_over_is_stopped_and_the_round_starts() {
  let world = World::new();
  let mut env = world.env();
  // The scope a finished round left: active, its launch directory gone, no process of the round
  // alive. The start still needs the name free, like a real systemd-run after a stop that landed.
  env.insert("FAKE_SCOPE_STATE".to_string(), "active".to_string());
  env.insert("FAKE_SYSTEMD_RUN_EXISTING".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start after the stale scope was stopped:\n{}", run.describe());
  let calls = world.systemctl_calls();
  assert!(
    calls.contains(&format!("is-active --quiet improve-{DIRECTION}.scope")),
    "the runner did not ask whether the scope is active:\n{calls}"
  );
  assert!(
    calls.contains(&format!("stop improve-{DIRECTION}.scope")),
    "the runner did not stop the stale scope:\n{calls}"
  );
  assert!(!calls.contains("reset-failed"), "the runner reset an active scope:\n{calls}");
  assert!(
    stderr(&run).contains(&format!("stopped the stale scope improve-{DIRECTION}.scope")),
    "the runner did not report the stale scope it stopped:\n{}",
    run.describe()
  );
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(scope.contains("--unit improve-perf"), "the round started no scope after the stop: {scope}");
}

#[test]
#[ignore = "integration"]
fn an_active_scope_with_a_launch_directory_but_no_leader_is_stopped_and_the_round_starts() {
  let world = World::new();
  // A launch directory alone is not a live round. Its wrapper removes it when it exits, and the
  // leader that would name it is gone. The scope's round is therefore over, and only a leftover
  // process keeps the unit loaded.
  let launch = world.remote_home.join("improve/launch/20261101T083100Z-1-100");
  fs::create_dir_all(&launch).expect("the finished round's launch directory");
  let mut env = world.env();
  env.insert("FAKE_SCOPE_STATE".to_string(), "active".to_string());
  env.insert("FAKE_SYSTEMD_RUN_EXISTING".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start after the stale scope was stopped:\n{}", run.describe());
  assert!(
    stderr(&run).contains(&format!("stopped the stale scope improve-{DIRECTION}.scope")),
    "the runner did not report the stale scope it stopped:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn an_active_scope_with_a_live_round_still_refuses_with_status_5() {
  let world = World::new();
  // The previous round: its launch directory, which its wrapper removes when it exits, and its
  // leader, the --in-scope bash that runs from that directory. Both alive, so the second start of
  // the direction refuses, and nothing stops the live round.
  let launch = world.remote_home.join("improve/launch/20261101T103100Z-1-200");
  fs::create_dir_all(&launch).expect("the live round's launch directory");
  let leader_script = launch.join("remote-runner.sh");
  fs::write(&leader_script, "#!/usr/bin/env bash\nsleep 30\n").expect("the leader script");
  let mut command = harness::command("bash", &world.env());
  command.arg(&leader_script).args(["--in-scope", "20261101T103100Z-1-200", DIRECTION]);
  let mut leader = harness::spawn(&mut command);
  let mut env = world.env();
  env.insert("FAKE_SCOPE_STATE".to_string(), "active".to_string());
  let run = world.run(&env);
  // The leader's `sleep` survives a lone kill of the bash that started it: stop the children first.
  let _ = std::process::Command::new("pkill").args(["-KILL", "-P", &leader.id().to_string()]).status();
  leader.kill().expect("kill the leader");
  let _ = leader.wait();
  assert_eq!(run.code, 5, "{}", run.describe());
  assert!(!world.claude_ran(), "a round started inside the live round's scope:\n{}", run.describe());
  assert!(stderr(&run).contains("previous round"), "{}", run.describe());
  let calls = world.systemctl_calls();
  assert!(
    !calls.contains(&format!("stop improve-{DIRECTION}.scope")),
    "the runner stopped a live round's scope:\n{calls}"
  );
  assert!(!calls.contains("reset-failed"), "the runner reset a live round's scope:\n{calls}");
  assert!(
    !world.systemd_run_log.exists(),
    "the runner tried to start a scope over the live round's:\n{}",
    world.systemd_run_log.display()
  );
}

#[test]
#[ignore = "integration"]
fn an_absent_scope_starts_without_a_reset() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let calls = world.systemctl_calls();
  assert!(
    calls.contains(&format!("is-failed --quiet improve-{DIRECTION}.scope")),
    "the runner did not ask for the scope's state:\n{calls}"
  );
  assert!(!calls.contains("reset-failed"), "the runner reset a scope it never found failed:\n{calls}");
  assert!(!stderr(&run).contains("cleared the failed scope"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_missing_direction_clone_exits_4_and_starts_no_claude() {
  let world = World::new();
  fs::remove_dir_all(world.remote_home.join("improve").join(DIRECTION).join("charlie-bot")).expect("remove the clone");
  let run = world.run(&world.env());
  assert_eq!(run.code, 4, "{}", run.describe());
  assert!(!world.claude_ran(), "the round started without its clone");
  assert!(stderr(&run).contains("clone"), "{}", run.describe());
  assert!(!world.systemd_run_log.exists(), "a scope started without the clone");
}

#[test]
#[ignore = "integration"]
fn a_clone_without_the_hooks_exits_4_and_starts_no_claude() {
  let world = World::new();
  let clone = world.remote_home.join("improve").join(DIRECTION).join("charlie-bot");
  let mut env = world.env();
  env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
  let mut command = harness::command("git", &env);
  command.arg("-C").arg(&clone).args(["config", "--unset", "core.hooksPath"]);
  let unset = harness::run(&mut command);
  assert_eq!(unset.code, 0, "{}", unset.describe());
  let run = world.run(&world.env());
  assert_eq!(run.code, 4, "{}", run.describe());
  assert!(!world.claude_ran(), "the round started in a clone without the hooks");
  assert!(stderr(&run).contains("core.hooksPath"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_heartbeat_stops_the_scope_after_the_launcher_is_killed() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_WAIT_FOR".to_string(), world.scope_stopped.display().to_string());
  // The fake ssh forwards standard input, so the launcher gets an input that ends, as `run` gives it.
  let mut command = world.launcher_command(&env);
  command.stdin(Stdio::null());
  let mut launcher = harness::spawn(&mut command);
  let started = wait_until(Duration::from_secs(30), || {
    world.launch_directory().is_some_and(|launch| launch.join("started").exists())
  });
  assert!(started, "the round never started a scope:\n{}", world.ssh_arguments());
  launcher.kill().expect("kill the launcher");
  let stopped = wait_until(Duration::from_secs(5), || world.systemctl_calls().contains("stop improve-perf.scope"));
  let calls = world.systemctl_calls();
  let _ = launcher.wait();
  assert!(stopped, "the heartbeat did not stop the scope within 5 seconds: {calls:?}");
  // The stopped scope ends the round's own shell, which then removes its launch directory. Wait for that, so the
  // orphaned remote side is done before the sandbox goes away.
  assert!(
    wait_until(Duration::from_secs(15), || world.launch_directory().is_none()),
    "the stopped round left its launch directory behind: {}",
    world.ssh_arguments()
  );
}

/// A fake live round of `direction`: a leader whose command line the runner's round state reads, beside the
/// launch directory that command line names. The caller stops the leader after the run.
fn spawn_live_round(world: &World, direction: &str) -> std::process::Child {
  let launch_id = "20261103T120000Z-9-900";
  let launch = world.remote_home.join("improve/launch").join(launch_id);
  fs::create_dir_all(&launch).expect("the live round's launch directory");
  fs::write(launch.join("remote-runner.sh"), "#!/usr/bin/env bash\nsleep 30\n").expect("the leader script");
  let mut env = harness::Env::new();
  env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
  let mut command = harness::command("bash", &env);
  command.arg(launch.join("remote-runner.sh")).args(["--in-scope", launch_id, direction]);
  harness::spawn(&mut command)
}

/// The stop of a leader that [`spawn_live_round`] started: the children first, since a lone kill of the
/// bash that started them leaves them running.
fn stop_live_round(leader: &mut std::process::Child) {
  let _ = std::process::Command::new("pkill").args(["-KILL", "-P", &leader.id().to_string()]).status();
  leader.kill().expect("stop the fake round's leader");
  let _ = leader.wait();
}

#[test]
#[ignore = "integration"]
fn a_running_rounds_clone_adds_its_checked_out_branchs_files_to_the_list() {
  let world = World::new();
  // A standalone clone of another direction, like the host worker's five clones on the remote host.
  let clone = world.other_direction_clone("ux");
  // A file whose removal main lands after the topic branch's base, as a rename's old name does.
  fs::write(clone.join("dropped.md"), "old\n").expect("the dropped fixture file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "add the dropped file"]);
  // The committed change rides the checked-out topic branch, as a round's branch does, and it
  // touches the old path beside the live one.
  world.git(&clone, &["switch", "-q", "-c", "improve/ux/topic"]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed fixture file");
  fs::write(clone.join("dropped.md"), "topic\n").expect("the dropped path's topic change");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic topic"]);
  // main deletes the old path, as a rename's landing does.
  world.git(&clone, &["switch", "-q", "main"]);
  world.git(&clone, &["rm", "-q", "dropped.md"]);
  world.git(&clone, &["commit", "-q", "-m", "drop the old name"]);
  world.git(&clone, &["push", "-q", "origin", "main"]);
  world.git(&clone, &["switch", "-q", "improve/ux/topic"]);
  // The uncommitted change sits in the checkout. The untracked file has no git entry at all.
  fs::write(clone.join("base.md"), "changed\n").expect("the uncommitted change");
  fs::write(clone.join("dropped.md"), "uncommitted\n").expect("the dropped path's uncommitted change");
  fs::write(clone.join("untracked.md"), "new\n").expect("the untracked file");
  // The ux round is running while this perf round builds its list, as a burst's rounds do.
  let mut leader = spawn_live_round(&world, "ux");

  let run = world.run(&world.env());

  stop_live_round(&mut leader);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  // Every path the running ux round's clone holds prints with its branch: the committed paths
  // carry unmerged commits, and the uncommitted and untracked paths are uncommitted work.
  for carried in [
    "committed.md\timprove/ux/topic",
    "base.md\timprove/ux/topic",
    "dropped.md\timprove/ux/topic",
    "untracked.md\timprove/ux/topic",
  ] {
    assert!(
      list.lines().any(|line| line == carried),
      "the in-flight list misses the {carried} change of the running ux round's clone:\n{list}"
    );
  }
}

#[test]
#[ignore = "integration"]
fn an_open_pull_requests_files_appear_in_the_in_flight_list() {
  let world = World::new();
  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "12\tcharliebot/task-open\tpr-only.md\n").expect("the open pull requests");
  let mut env = world.env();
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(
    list.lines().any(|line| line == "pr-only.md\tcharliebot/task-open"),
    "the in-flight list misses an open pull request's file:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn a_failing_pull_request_files_api_call_stops_the_list_with_an_error() {
  let world = World::new();
  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "12\tcharliebot/task-open\tpr-only.md\n").expect("the open pull requests");
  let mut env = world.env();
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());
  env.insert("FAKE_GH_FAIL_FILES".to_string(), "1".to_string());

  let run = run_inflight_list_with_env(&world, &env);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("cannot read the open pull request condition for PR 12"),
    "the error does not name the PR condition:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_squash_merged_branch_of_an_unowned_worktree_stays_claimed() {
  let world = World::new();
  // A leftover branch of a round that already landed, checked out in its worktree as the host leaves it.
  let checkout = world.sandbox.root().join("stale-checkout");
  world.git(&world.repo, &["branch", "charliebot/stale"]);
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/stale"]);
  fs::write(checkout.join("stale.md"), "landed\n").expect("the landed topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic stale topic"]);
  // origin/main receives the topic as a squash merge, and a later change moves the same file on, as a later
  // round does. The launcher's tracking ref still points behind both, so only the script's fetch sees them.
  world.git(&world.repo, &["merge", "-q", "--squash", "charliebot/stale"]);
  world.git(&world.repo, &["commit", "-q", "-m", "land the stale topic"]);
  fs::write(world.repo.join("stale.md"), "landed\nand moved on\n").expect("the later change");
  world.git(&world.repo, &["commit", "-q", "-am", "move the landed file on"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(list.lines().any(|line| line == "stale.md"), "the unowned worktree drops its landed branch file:\n{list}");
}

#[test]
#[ignore = "integration"]
fn an_unlanded_branch_names_its_file_even_when_origin_main_has_changed_it() {
  let world = World::new();
  // A leftover branch whose commit never reached origin/main, checked out in its worktree as the host leaves it.
  let checkout = world.sandbox.root().join("unlanded-checkout");
  world.git(&world.repo, &["branch", "charliebot/unlanded"]);
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/unlanded"]);
  fs::write(checkout.join("shared.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic unlanded topic"]);
  // Main changes the same file after the branch, while the unowned worktree keeps its committed path.
  fs::write(world.repo.join("shared.md"), "another change\n").expect("the other change");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "change the shared file on main"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(
    list.lines().any(|line| line == "shared.md\tcharliebot/unlanded"),
    "the in-flight list misses an unlanded branch's file that origin/main changed:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn a_branch_does_not_claim_a_path_it_restores_to_its_merge_base() {
  let world = World::new();
  fs::write(world.repo.join("restored.md"), "base\n").expect("the base file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "add the base file"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  let checkout = world.sandbox.root().join("restored-checkout");
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", "charliebot/restored"]);

  fs::write(checkout.join("restored.md"), "temporary change\n").expect("the temporary change");
  world.git(&checkout, &["commit", "-q", "-am", "change the file"]);
  fs::write(checkout.join("restored.md"), "base\n").expect("restore the base contents");
  world.git(&checkout, &["commit", "-q", "-am", "restore the file"]);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    !lists_path(&run.stdout, "restored.md"),
    "the branch claims a path that its final tree restores to the merge base:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn an_unowned_worktree_branch_keeps_all_changed_paths() {
  let world = World::new();
  // The branch changes four paths: two additions and two deletions.
  fs::write(world.repo.join("base.md"), "base\n").expect("the base file");
  fs::write(world.repo.join("together.md"), "together\n").expect("the together file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "add the deletable files"]);
  world.git(&world.repo, &["branch", "charliebot/partial"]);
  let checkout = world.sandbox.root().join("partial-checkout");
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/partial"]);
  // Main later lands one added path and one deletion, but the unowned worktree keeps all four changes.
  fs::write(checkout.join("landed.md"), "landed\n").expect("the landed topic's file");
  fs::write(checkout.join("unlanded.md"), "unlanded\n").expect("the unlanded topic's file");
  fs::remove_file(checkout.join("base.md")).expect("remove the base file");
  fs::remove_file(checkout.join("together.md")).expect("remove the together file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic partial topic"]);
  // Main later lands the added path, changes it again, and deletes one path from the branch.
  fs::write(world.repo.join("landed.md"), "landed\n").expect("the landed version");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "land the partial topic's file"]);
  fs::write(world.repo.join("landed.md"), "landed\nand moved on\n").expect("the later change");
  world.git(&world.repo, &["commit", "-q", "-am", "move the landed file on"]);
  fs::remove_file(world.repo.join("together.md")).expect("remove the together file on main");
  world.git(&world.repo, &["commit", "-q", "-am", "drop the file the branch drops too"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  let mut listed: Vec<&str> = list.lines().collect();
  listed.sort_unstable();
  // The branch's one commit carries an unmerged patch, so all four paths it touches print with the
  // branch that carries them.
  assert_eq!(
    listed,
    [
      "base.md\tcharliebot/partial",
      "landed.md\tcharliebot/partial",
      "together.md\tcharliebot/partial",
      "unlanded.md\tcharliebot/partial"
    ],
    "the unowned branch loses a changed path:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn an_unowned_rebased_branch_keeps_its_committed_path() {
  let world = World::new();
  // A leftover branch of a round that a rebase landed, checked out in its worktree as the host leaves it.
  let checkout = world.sandbox.root().join("rebased-checkout");
  world.git(&world.repo, &["branch", "charliebot/rebased"]);
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/rebased"]);
  fs::write(checkout.join("rebased.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic rebased topic"]);
  // Main commits the same subject with a resolved file value.
  fs::write(world.repo.join("rebased.md"), "topic, resolved\n").expect("the resolved file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "synthetic rebased topic"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  // Main's resolved commit is not the branch's patch, so the branch still carries an unmerged
  // commit for the path, and the path prints with that branch.
  assert!(
    list.lines().any(|line| line == "rebased.md\tcharliebot/rebased"),
    "the unowned worktree drops its committed path:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn an_idle_clones_branches_name_no_path_and_the_branch_stays() {
  let world = World::new();
  let clone = world.other_direction_clone("ux");
  // The checked-out branch of a round that ended, with its committed, uncommitted and untracked
  // paths still in the checkout, as the host leaves a finished clone.
  world.git(&clone, &["switch", "-q", "-c", "improve/ux/ended"]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic ended topic"]);
  fs::write(clone.join("dirty.md"), "changed\n").expect("the uncommitted change");
  fs::write(clone.join("untracked.md"), "new\n").expect("the untracked file");
  // An older round's branch that no checkout carries.
  world.git(&clone, &["switch", "-q", "main"]);
  world.git(&clone, &["switch", "-q", "-c", "improve/ux/old"]);
  fs::write(clone.join("old.md"), "old\n").expect("the old branch's file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic old topic"]);
  world.git(&clone, &["switch", "-q", "improve/ux/ended"]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(list.trim().is_empty(), "the idle clone still claims paths without a running round:\n{list}");
  assert!(
    !world.git(&clone, &["branch", "--list", "improve/ux/old"]).trim().is_empty(),
    "the old branch left the clone"
  );
}

#[test]
#[ignore = "integration"]
fn a_running_rounds_clone_keeps_all_changed_paths() {
  let world = World::new();
  let clone = world.other_direction_clone("ux");
  // The topic adds two files and deletes `base.md`, which origin/main never drops.
  world.git(&clone, &["switch", "-q", "-c", "improve/ux/partial"]);
  fs::write(clone.join("landed.md"), "landed\n").expect("the landed topic's file");
  fs::write(clone.join("unlanded.md"), "unlanded\n").expect("the unlanded topic's file");
  fs::remove_file(clone.join("base.md")).expect("remove the base file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic partial topic"]);
  // Main later lands the added path, then changes it again.
  let origin = world.other_direction_origin("ux").display().to_string();
  let landing = world.sandbox.dir("landing/ux");
  world.git(&world.sandbox.dir("landing"), &["clone", "-q", &origin, "ux"]);
  fs::write(landing.join("landed.md"), "landed\n").expect("the landed version");
  world.git(&landing, &["add", "-A"]);
  world.git(&landing, &["commit", "-q", "-m", "land the partial topic's file"]);
  fs::write(landing.join("landed.md"), "landed\nand moved on\n").expect("the later change");
  world.git(&landing, &["commit", "-q", "-am", "move the landed file on"]);
  world.git(&landing, &["push", "-q", "origin", "main"]);
  // The ux round is running while this perf round builds its list.
  let mut leader = spawn_live_round(&world, "ux");

  let run = world.run(&world.env());

  stop_live_round(&mut leader);
  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  let mut listed: Vec<&str> = list.lines().collect();
  listed.sort_unstable();
  assert_eq!(
    listed,
    ["base.md\timprove/ux/partial", "landed.md\timprove/ux/partial", "unlanded.md\timprove/ux/partial"],
    "the live clone loses a changed path:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn a_process_in_another_directions_clone_keeps_its_checked_out_branchs_paths() {
  let world = World::new();
  let clone = world.other_direction_clone("ux");
  world.git(&clone, &["switch", "-q", "-c", "improve/ux/live"]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic live topic"]);
  fs::write(clone.join("dirty.md"), "changed\n").expect("the uncommitted change");
  // No ux round runs, but a process works in the clone, as a round's worker does.
  let mut process = std::process::Command::new("bash")
    .arg("-c")
    .arg("exec sleep 30")
    .current_dir(&clone)
    .spawn()
    .expect("the clone process");
  let cwd = format!("/proc/{}/cwd", process.id());
  assert!(
    wait_until(Duration::from_secs(5), || fs::read_link(&cwd).is_ok_and(|target| target == clone)),
    "the clone process did not enter the clone"
  );

  let run = world.run(&world.env());

  process.kill().expect("stop the clone process");
  process.wait().expect("reap the clone process");
  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  for named in ["committed.md\timprove/ux/live", "dirty.md\timprove/ux/live"] {
    assert!(list.lines().any(|line| line == named), "the in-flight list drops the process-held path {named}:\n{list}");
  }
}

#[test]
#[ignore = "integration"]
fn an_other_directions_clone_whose_fetch_fails_stops_the_round_with_exit_1() {
  let world = World::new();
  let clone = world.other_direction_clone("ux");
  // The clone's origin is gone, so the fetch of its origin/main fails. The round must stop rather
  // than build its in-flight list without the ux clone's files.
  let gone = world.sandbox.root().join("gone.git").display().to_string();
  world.git(&clone, &["remote", "set-url", "origin", &gone]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(
    stderr(&run).contains(&format!("cannot fetch origin/main in {}", clone.display())),
    "the round did not name the clone it could not fetch:\n{}",
    run.describe()
  );
  assert!(!world.claude_ran(), "the round started without the ux clone's in-flight files");
}

#[test]
#[ignore = "integration"]
fn the_runners_own_checkout_contributes_no_files() {
  let world = World::new();
  // The runner's checkout, with mainline work that its remote's main does not hold.
  let runner_root = world.remote_home.join("improve/runner");
  let clone = runner_root.join("charlie-bot");
  world.git(&world.sandbox.dir("seeds/runner"), &["init", "-q", "-b", "main"]);
  fs::write(world.sandbox.root().join("seeds/runner/seed.md"), "seed\n").expect("the seed's file");
  world.git(&world.sandbox.dir("seeds/runner"), &["add", "-A"]);
  world.git(&world.sandbox.dir("seeds/runner"), &["commit", "-q", "-m", "synthetic base"]);
  let origin = world.sandbox.dir("remotes/runner.git").display().to_string();
  world.git(&world.sandbox.dir("seeds/runner"), &["clone", "-q", "--bare", ".", &origin]);
  world.git(&runner_root, &["clone", "-q", &origin, "charlie-bot"]);
  fs::write(clone.join("runner-mainline.md"), "runner\n").expect("the runner's mainline file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic runner mainline"]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(
    !list.lines().any(|line| line == "runner-mainline.md"),
    "the in-flight list names the runner's own checkout:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn the_runner_builds_the_round_tools_with_its_own_cargo_home_and_tmpdir_and_the_launchers_origin() {
  let world = World::new();
  // The commit the runner builds needs a rust/ directory, and it reaches the launcher's origin, which the runner
  // clones from.
  fs::create_dir_all(world.repo.join("rust")).expect("the rust directory");
  fs::write(world.repo.join("rust/Cargo.toml"), "[workspace]\n").expect("the workspace file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "synthetic build commit"]);
  let commit = world.git(&world.repo, &["rev-parse", "HEAD"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  let origin_text = world.other_direction_origin("runtime").display().to_string();
  let cargo_env = world.sandbox.root().join("cargo-env.txt");
  let mut env = world.env();
  env.insert("FAKE_CARGO_ENV".to_string(), cargo_env.display().to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 0, "{}", run.describe());
  let recorded = fs::read_to_string(&cargo_env).expect("the fake cargo ran");
  let cargo_home = world.remote_home.join("improve/runner/cargo-home");
  let tmp = world.remote_home.join("improve/runner/tmp");
  assert!(recorded.contains(&format!("CARGO_HOME={}\n", cargo_home.display())), "{recorded}");
  assert!(recorded.contains(&format!("TMPDIR={}\n", tmp.display())), "{recorded}");
  let runner_bin = world.remote_home.join("improve/runner/bin");
  for tool in ["charliebot", "improve-sandbox", "mock-home"] {
    assert!(runner_bin.join(format!("{tool}-{commit}")).is_file(), "the runner did not install {tool}");
  }
  let clone = world.remote_home.join("improve/runner/charlie-bot");
  assert_eq!(world.git(&clone, &["remote", "get-url", "origin"]), origin_text, "the runner cloned another origin");
}

#[test]
#[ignore = "integration"]
fn two_concurrent_runners_build_tools_once_and_preserve_one_shared_mock_home() {
  let world = World::new();
  // The commit both rounds need carries a rust/ directory, as the build's `cd` needs, and sits on
  // an origin the runner can clone from.
  fs::create_dir_all(world.repo.join("rust")).expect("the rust directory");
  fs::write(world.repo.join("rust/Cargo.toml"), "[workspace]\n").expect("the workspace file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "synthetic build commit"]);
  let commit = world.git(&world.repo, &["rev-parse", "HEAD"]);
  let origin = world.sandbox.dir("remotes/build/charlie-bot.git");
  let origin_text = origin.display().to_string();
  world.git(&world.repo, &["clone", "-q", "--bare", ".", &origin_text]);
  let cargo_log = world.sandbox.root().join("cargo-builds.log");
  let cargo_env = world.sandbox.root().join("cargo-env.txt");
  let mock_home_barrier = world.sandbox.dir("mock-home-barrier");
  fs::write(
    world.remote_home.join("improve/home/credentials.yaml"),
    format!("{REVIEW_CREDENTIAL}:\n  api_key: {REVIEW_KEY}\n"),
  )
  .expect("the round's credentials file");
  // The round home keeps the pattern file of the previous round, as the host's does. Both
  // runners' copies therefore take the in-place path instead of racing on a fresh file.
  fs::write(world.remote_home.join("improve/home/skills_leak_patterns.local.txt"), "").expect("the pattern file");

  // Two directions start in the same second, and both need the commit that the runner bin lacks.
  // Each direction brings its own clone, whose origin the other direction's runner can fetch, and
  // its own launch directory, records and TMPDIR.
  let mut commands = Vec::new();
  for (index, direction) in [DIRECTION, "ux"].into_iter().enumerate() {
    let bare = world.sandbox.dir(&format!("remotes/{index}/charlie-bot.git"));
    let bare_text = bare.display().to_string();
    world.git(&world.repo, &["clone", "-q", "--bare", ".", &bare_text]);
    let round_root = world.remote_home.join("improve").join(direction);
    let clone = round_root.join("charlie-bot");
    if direction == DIRECTION {
      // The fixture clone's origin names no fetchable repository, and the other direction's
      // runner fetches in this clone, so the origin becomes the local bare one.
      world.git(&clone, &["remote", "set-url", "origin", &bare_text]);
    } else {
      fs::create_dir_all(&round_root).expect("the ux round root");
      world.git(&round_root, &["clone", "-q", &bare_text, "charlie-bot"]);
      world.git(&clone, &["config", "core.hooksPath", "dev/git-hooks"]);
    }
    let launch_id = format!("20261102T090000Z-1-40{index}");
    let launch = world.stage_launch(&launch_id);
    harness::copy_file(&harness::repo_root().join("dev/improve/remote-runner.sh"), &launch.join("remote-runner.sh"));
    // The launcher sends the rule script beside the runner, so the runner's clone scan sources it.
    harness::copy_file(&harness::repo_root().join("dev/improve/inflight-files.sh"), &launch.join("inflight-files.sh"));
    // The launcher sends the review's charlie-code beside the runner, which installs it and sources its credential_key.
    harness::copy_file(
      &harness::repo_root().join("dev/improve/review-charlie-code.sh"),
      &launch.join("review-charlie-code.sh"),
    );
    fs::write(launch.join("args"), "").expect("the launch arguments");
    write_member_credential(&launch, &[("ANTHROPIC_AUTH_TOKEN", GATEWAY_TOKEN)]);
    fs::write(
      launch.join("env"),
      format!(
        "IMPROVE_REVIEW_CREDENTIAL={REVIEW_CREDENTIAL}\0IMPROVE_REVIEW_MODEL={REVIEW_MODEL}\0\
         IMPROVE_REVIEW_API_BASE={REVIEW_API_BASE}\0"
      ),
    )
    .expect("the launch environment");
    fs::write(launch.join("paths"), "").expect("the launch paths");
    fs::write(launch.join("origin"), format!("{origin_text}\n")).expect("the launch origin");
    fs::write(launch.join("skills_leak_patterns.local.txt"), "fictionalmarker\n").expect("the leak patterns");
    fs::create_dir_all(format!("/tmp/improve-{direction}-tmp")).expect("the round's TMPDIR");
    let records = world.sandbox.dir(&format!("records-{direction}"));
    let mut env = harness::Env::new();
    env.insert("HOME".to_string(), world.remote_home.display().to_string());
    env.insert(
      "PATH".to_string(),
      format!("{}:/usr/local/bin:/usr/bin:/bin", world.remote_home.join(".local/bin").display()),
    );
    env.insert("FAKE_CLAUDE_ARGV".to_string(), records.join("claude-argv.txt").display().to_string());
    env.insert("FAKE_CLAUDE_ENV".to_string(), records.join("claude-env.txt").display().to_string());
    env.insert("FAKE_SYSTEMCTL_LOG".to_string(), world.systemctl_log.display().to_string());
    env.insert("FAKE_SYSTEMD_RUN_LOG".to_string(), world.systemd_run_log.display().to_string());
    env.insert("FAKE_SCOPE_STOPPED".to_string(), world.scope_stopped.display().to_string());
    env.insert("FAKE_CARGO_ENV".to_string(), cargo_env.display().to_string());
    env.insert("FAKE_CARGO_LOG".to_string(), cargo_log.display().to_string());
    env.insert("FAKE_MOCK_HOME_LOG".to_string(), world.mock_home_log.display().to_string());
    env.insert("FAKE_MOCK_HOME_FIXTURE".to_string(), world.mock_home_fixture.display().to_string());
    env.insert(
      "FAKE_MOCK_HOME_DIR".to_string(),
      world.remote_home.join("improve/mock-home").join(world::MOCK_HOME_VERSION).display().to_string(),
    );
    env.insert("FAKE_CLAUDE_BARRIER".to_string(), mock_home_barrier.display().to_string());
    env.insert("FAKE_VENDOR_ROOT".to_string(), world.vendor_root.display().to_string());
    env.insert("FAKE_CURL_LOG".to_string(), world.curl_log.display().to_string());
    env.insert("FAKE_CLI_RUNS".to_string(), world.cli_runs.display().to_string());
    // The build runs long enough that the second runner arrives while the first one holds the
    // work, as two burst starts do on the host.
    env.insert("FAKE_CARGO_SLEEP".to_string(), "2".to_string());
    env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
    env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
    let mut command = harness::command("bash", &env);
    command.arg(launch.join("remote-runner.sh")).args([
      &launch_id,
      direction,
      &commit,
      "claude",
      world::LOCAL_CLAUDE_VERSION,
    ]);
    commands.push(command);
  }

  let runs: Vec<harness::Run> = std::thread::scope(|scope| {
    commands
      .iter_mut()
      .map(|command| scope.spawn(move || harness::run(command)))
      .collect::<Vec<_>>()
      .into_iter()
      .map(|handle| handle.join().expect("the runner thread"))
      .collect()
  });

  for run in &runs {
    assert_eq!(run.code, 0, "{}", run.describe());
  }
  let mock_home = world.remote_home.join("improve/mock-home").join(world::MOCK_HOME_VERSION);
  let mock_home_calls = fs::read_to_string(&world.mock_home_log).expect("the mock-home calls");
  assert_eq!(mock_home_calls.lines().filter(|line| line.starts_with("generate ")).count(), 1, "{mock_home_calls}");
  assert_eq!(
    fs::read_to_string(mock_home.join("manifest.json")).expect("the shared mock-home manifest"),
    "synthetic complete mock home\n"
  );
  for direction in [DIRECTION, "ux"] {
    assert!(mock_home_barrier.join(direction).is_file(), "the {direction} round did not read alongside its peer");
  }
  let builds = fs::read_to_string(&cargo_log).unwrap_or_default();
  assert_eq!(builds.lines().count(), 1, "the runners built the commit more than once");
  let runner_bin = world.remote_home.join("improve/runner/bin");
  for tool in ["charliebot", "improve-sandbox", "mock-home"] {
    assert!(runner_bin.join(format!("{tool}-{commit}")).is_file(), "the runners did not install {tool}");
    assert_eq!(
      fs::read_link(runner_bin.join(tool)).expect("the runner's tool link"),
      std::path::PathBuf::from(format!("{tool}-{commit}")),
      "a round's {tool} link names another commit"
    );
  }
  for (direction, run) in std::iter::zip([DIRECTION, "ux"], &runs) {
    let records = world.sandbox.dir(&format!("records-{direction}"));
    let ran = records.join("claude-argv.txt");
    assert!(ran.is_file(), "the {direction} round did not run the tools of the commit:\n{}", run.describe());
    let env = fs::read_to_string(records.join("claude-env.txt")).expect("the round's environment");
    // The round's PATH starts with its member CLI's bin, then the tools of this commit.
    let commit_bin = runner_bin.join(&commit).display().to_string();
    let member_bin =
      runner_bin.parent().expect("the runner root").join(format!("members/claude-{}/bin", world::LOCAL_CLAUDE_VERSION));
    let pinned = format!("PATH={}:{commit_bin}:", member_bin.display());
    assert!(env.contains(&pinned), "the {direction} round did not pin its tools:\n{env}");
  }
}

#[test]
#[ignore = "integration"]
fn another_clones_locked_origin_main_ref_does_not_stop_the_rounds_in_flight_read() {
  let world = World::new();
  let clone = world.other_direction_clone("ux");
  // The committed change rides a topic branch, as a round's branch does.
  world.git(&clone, &["switch", "-q", "-c", "improve/ux/topic"]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed fixture file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic topic"]);
  // The origin's main moves ahead, so the runner's fetch has a ref to write, as a live host's does.
  world.git(&clone, &["switch", "-q", "main"]);
  fs::write(clone.join("advanced.md"), "later\n").expect("the main commit's file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "advance main"]);
  world.git(&clone, &["push", "-q", "origin", "main"]);
  world.git(&clone, &["switch", "-q", "improve/ux/topic"]);
  // The uncommitted change sits in the checkout. The untracked file has no git entry at all.
  fs::write(clone.join("base.md"), "changed\n").expect("the uncommitted change");
  fs::write(clone.join("untracked.md"), "new\n").expect("the untracked file");
  // The clone's own origin/main stays behind, so the fetch updates it, and a concurrent writer
  // holds the ref lock while it does.
  let behind = world.git(&clone, &["rev-parse", "HEAD~1"]);
  world.git(&clone, &["update-ref", "refs/remotes/origin/main", &behind]);
  fs::create_dir_all(clone.join(".git/refs/remotes/origin")).expect("the ref directory");
  fs::write(clone.join(".git/refs/remotes/origin/main.lock"), "").expect("the ref lock");

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  // The clone's checked-out topic branch claims nothing: no ux round runs, so the read reaches
  // only the fetch, which the locked ref must not stop.
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(list.trim().is_empty(), "the locked clone still claimed paths:\n{list}");
  assert!(
    !clone.join(".git/FETCH_HEAD").exists(),
    "the runner wrote the clone's shared FETCH_HEAD:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_locked_origin_main_ref_does_not_stop_the_launchers_in_flight_list() {
  let world = World::new();
  // The origin's main moves ahead of the checkout's tracking ref, so the script's fetch has a ref
  // to write, as a live host's does.
  fs::write(world.repo.join("advanced.md"), "later\n").expect("the main commit's file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "advance main"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);
  // A worktree branch whose work origin/main does not hold, as a live line's checkout is.
  let checkout = world.sandbox.root().join("locked-checkout");
  world.git(&world.repo, &["branch", "charliebot/locked"]);
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/locked"]);
  fs::write(checkout.join("locked.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic locked topic"]);
  fs::write(checkout.join("dirty.md"), "uncommitted\n").expect("the uncommitted change");
  // The lock a concurrent fetch of this checkout holds while it writes the shared tracking ref.
  fs::create_dir_all(world.repo.join(".git/refs/remotes/origin")).expect("the ref directory");
  fs::write(world.repo.join(".git/refs/remotes/origin/main.lock"), "").expect("the ref lock");
  let fetch_head = world.repo.join(".git/FETCH_HEAD");
  fs::remove_file(&fetch_head).expect("remove the setup fetch's FETCH_HEAD");

  let mut command = harness::command("bash", &world.env());
  // The launcher runs the script out of its checkout, so this run does too.
  command.arg(world.repo.join("dev/improve/inflight-files.sh")).arg("improve/perf/");
  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["locked.md\tcharliebot/locked", "dirty.md\tcharliebot/locked"] {
    assert!(run.stdout.lines().any(|line| line == listed), "the in-flight list misses {listed}:\n{}", run.describe());
  }
  assert!(!fetch_head.exists(), "the launcher's in-flight list wrote the shared FETCH_HEAD");
}

#[test]
#[ignore = "integration"]
fn an_unowned_worktree_keeps_a_path_that_main_deleted() {
  let world = World::new();
  // Two tracked files on main, as a live repository's paths are.
  fs::write(world.repo.join("old.md"), "old\n").expect("the old path's file");
  fs::write(world.repo.join("live.md"), "live\n").expect("the live path's file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "add the fixture files"]);
  // A worktree branch that changes both paths, as a live line's checkout is.
  let checkout = world.sandbox.root().join("renamed-checkout");
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", "charliebot/renamed"]);
  fs::write(checkout.join("old.md"), "topic\n").expect("the old path's topic change");
  fs::write(checkout.join("live.md"), "topic\n").expect("the live path's topic change");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic rename topic"]);
  // main deletes the old path after the branch's base, as a rename's landing does.
  world.git(&world.repo, &["rm", "-q", "old.md"]);
  world.git(&world.repo, &["commit", "-q", "-m", "drop the old name"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  // The worktree directory still carries the old path's uncommitted change, beside a live one and
  // an untracked file. An open pull request holds its own path.
  fs::write(checkout.join("old.md"), "uncommitted\n").expect("the old path's uncommitted change");
  fs::write(checkout.join("live.md"), "uncommitted\n").expect("the live path's uncommitted change");
  fs::write(checkout.join("fresh.md"), "untracked\n").expect("the untracked file");
  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "13\tcharliebot/task-open\tpr-only.md\n").expect("the open pull requests");
  let mut env = world.env();
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());

  let mut command = harness::command("bash", &env);
  command.arg(world.repo.join("dev/improve/inflight-files.sh")).arg("improve/perf/");
  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  // No task names this worktree, so its committed and dirty paths stay alongside the open pull request's file.
  // Each path prints with the branch that carries it. An unmerged commit's branch carries its file. The
  // dirty worktree's branch carries its uncommitted paths. An open pull request's head branch carries its file.
  let mut listed: Vec<&str> = run.stdout.lines().collect();
  listed.sort_unstable();
  assert_eq!(
    listed,
    [
      "fresh.md\tcharliebot/renamed",
      "live.md\tcharliebot/renamed",
      "old.md\tcharliebot/renamed",
      "pr-only.md\tcharliebot/task-open"
    ],
    "the in-flight list drops a path from an unowned worktree:\n{}",
    run.describe()
  );
}

/// One task lifecycle state written to the profile home's task store.
fn write_task_state(world: &World, task_id: &str, open: bool) -> std::path::PathBuf {
  let session_dir = world.profile_home.join(format!("sessions/{task_id}"));
  fs::create_dir_all(session_dir.join("data")).expect("the task session directory");
  let metadata = serde_json::json!({"id": task_id});
  fs::write(session_dir.join("metadata.json"), format!("{metadata:#}\n")).expect("the task metadata");
  let event = if open {
    serde_json::json!({"id": format!("{task_id}-state"), "type": "task_reopened"})
  } else {
    serde_json::json!({"id": format!("{task_id}-state"), "type": "task_closed", "outcome": "completed"})
  };
  let events = session_dir.join("data/chat_events.jsonl");
  fs::write(&events, format!("{event}\n")).expect("the task lifecycle event");
  events
}

/// A run record that links one task to its worktree. Its end time does not set the task state.
fn write_run_record(world: &World, task_id: &str, run_id: &str, worktree: &std::path::Path) {
  let run_dir = world.profile_home.join(format!("sessions/{task_id}/data/runs/{run_id}"));
  fs::create_dir_all(&run_dir).expect("the run record directory");
  let record = serde_json::json!({
    "id": run_id,
    "worktree_path": worktree.display().to_string(),
    "started_at": "2026-11-10T01:00:00.000000Z",
    "ended_at": "2026-11-10T02:00:00.000000Z",
  });
  fs::write(run_dir.join("metadata.json"), format!("{record:#}\n")).expect("the run record");
}

/// A task record that links one worktree to its owning session, as the store's `threads/` records do.
/// The session's run records may be gone with the session they belong to.
fn write_task_record(world: &World, owner_id: &str, thread_id: &str, worktree: &std::path::Path) {
  let record_dir = world.profile_home.join(format!("sessions/{owner_id}/threads/{thread_id}"));
  fs::create_dir_all(&record_dir).expect("the task record directory");
  let record = serde_json::json!({
    "id": thread_id,
    "session_id": owner_id,
    "status": "completed",
    "worktree_path": worktree.display().to_string(),
  });
  fs::write(record_dir.join("metadata.json"), format!("{record:#}\n")).expect("the task record");
}

/// One landed branch with dirty, staged and untracked paths, as a task worktree can hold after its commits land.
fn landed_worktree_with_uncommitted_state(world: &World, name: &str) -> std::path::PathBuf {
  let checkout = world.sandbox.root().join(name);
  world
    .git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", &format!("charliebot/{name}")]);
  fs::write(checkout.join("topic.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", &format!("synthetic {name} topic")]);
  fs::write(world.repo.join("topic.md"), "topic\n").expect("the squash commit's content");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", &format!("synthetic {name} topic, retitled (#9)")]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  fs::write(checkout.join("topic.md"), "uncommitted\n").expect("the uncommitted change");
  fs::write(checkout.join("staged.md"), "staged\n").expect("the staged change");
  world.git(&checkout, &["add", "staged.md"]);
  fs::write(checkout.join("untracked.md"), "untracked\n").expect("the untracked file");
  checkout
}

/// One unlanded branch with both a committed path and a dirty path.
fn unlanded_worktree_with_uncommitted_state(world: &World, name: &str) -> std::path::PathBuf {
  let checkout = world.sandbox.root().join(name);
  world
    .git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", &format!("charliebot/{name}")]);
  fs::write(checkout.join("topic.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", &format!("synthetic {name} topic")]);
  fs::write(checkout.join("dirty.md"), "uncommitted\n").expect("the uncommitted change");
  checkout
}

/// The in-flight list's run out of its checkout, as the launcher does.
fn run_inflight_list_with_env(world: &World, env: &harness::Env) -> harness::Run {
  let mut command = harness::command("bash", env);
  command.arg(world.repo.join("dev/improve/inflight-files.sh")).arg("improve/perf/");
  harness::run(&mut command)
}

fn run_inflight_list(world: &World) -> harness::Run {
  run_inflight_list_with_env(world, &world.env())
}

#[test]
#[ignore = "integration"]
fn an_open_task_keeps_an_unlanded_worktrees_paths_claimed_after_its_run_ends() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "open-task");
  write_task_state(&world, "task-open", true);
  write_run_record(&world, "task-open", "run-open-ended", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md\tcharliebot/open-task", "dirty.md\tcharliebot/open-task"] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops the open task's path {listed} after its run ended:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_closed_task_without_other_claimants_drops_an_unlanded_worktrees_paths() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "closed-task");
  write_task_state(&world, "task-closed", false);
  write_run_record(&world, "task-closed", "run-closed", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md", "dirty.md"] {
    assert!(
      !lists_path(&run.stdout, listed),
      "the in-flight list keeps the closed task's path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_closed_task_drops_a_landed_worktrees_paths() {
  let world = World::new();
  let checkout = landed_worktree_with_uncommitted_state(&world, "closed-landed-task");
  write_task_state(&world, "task-closed-landed", false);
  write_run_record(&world, "task-closed-landed", "run-closed-landed", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md", "staged.md", "untracked.md"] {
    assert!(
      !lists_path(&run.stdout, listed),
      "the in-flight list keeps the closed landed task's path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn an_open_pull_request_keeps_a_closed_tasks_worktree_paths_claimed() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "closed-pr");
  write_task_state(&world, "task-closed-pr", false);
  write_run_record(&world, "task-closed-pr", "run-closed-pr", &checkout);
  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "17\tcharliebot/closed-pr\tpr-only.md\n").expect("the open pull request");
  let mut env = world.env();
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());

  let run = run_inflight_list_with_env(&world, &env);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md\tcharliebot/closed-pr", "dirty.md\tcharliebot/closed-pr", "pr-only.md\tcharliebot/closed-pr"]
  {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the open pull request does not keep its worktree path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_worktree_without_a_task_name_keeps_its_committed_and_uncommitted_paths() {
  let world = World::new();
  landed_worktree_with_uncommitted_state(&world, "no-task");

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  // The topic commit is patch-equivalent on main, so the committed path prints bare. The same
  // path's uncommitted change, the staged path and the untracked path print with the branch that
  // carries them.
  for listed in
    ["topic.md", "topic.md\tcharliebot/no-task", "staged.md\tcharliebot/no-task", "untracked.md\tcharliebot/no-task"]
  {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops the unowned worktree path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_process_in_a_closed_tasks_worktree_keeps_its_paths_claimed() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "with-process");
  write_task_state(&world, "task-with-process", false);
  write_run_record(&world, "task-with-process", "run-with-process", &checkout);
  let mut process = std::process::Command::new("bash")
    .arg("-c")
    .arg("exec sleep 30")
    .current_dir(&checkout)
    .spawn()
    .expect("the worktree process");
  let cwd = format!("/proc/{}/cwd", process.id());
  assert!(
    wait_until(std::time::Duration::from_secs(5), || { fs::read_link(&cwd).is_ok_and(|target| target == checkout) }),
    "the worktree process did not enter the worktree"
  );

  let run = run_inflight_list(&world);

  process.kill().expect("stop the worktree process");
  process.wait().expect("reap the worktree process");
  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md\tcharliebot/with-process", "dirty.md\tcharliebot/with-process"] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops the process-held worktree path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn an_unreadable_task_state_stops_the_list_with_an_error() {
  use std::os::unix::fs::PermissionsExt;
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "unreadable-task");
  let events = write_task_state(&world, "task-unreadable", false);
  write_run_record(&world, "task-unreadable", "run-unreadable", &checkout);
  fs::set_permissions(&events, fs::Permissions::from_mode(0o000)).expect("the unreadable task state");
  assert!(fs::File::open(&events).is_err(), "the test needs task state this user cannot read");

  let run = run_inflight_list(&world);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("cannot read the task-state condition") && run.stderr.contains(&events.display().to_string()),
    "the in-flight list does not name the unreadable task state:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_closed_owning_sessions_task_record_drops_an_unlanded_worktrees_paths() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "closed-record");
  write_task_state(&world, "record-owner", false);
  write_task_record(&world, "record-owner", "thread-closed", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md", "dirty.md"] {
    assert!(
      !lists_path(&run.stdout, listed),
      "the in-flight list keeps the closed task record's path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn an_open_owning_sessions_task_record_keeps_the_worktree_paths_claimed() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "open-record");
  write_task_state(&world, "record-owner-open", true);
  write_task_record(&world, "record-owner-open", "thread-open", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md\tcharliebot/open-record", "dirty.md\tcharliebot/open-record"] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops the open task record's path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_launch_record_naming_the_worktree_claims_nothing() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "launch-record");
  write_task_state(&world, "record-owner-launch", false);
  // A trigger launch record beside the task records names the worktree as its worktree_path. The store
  // writes no such field there, and only the run and task records own a worktree. The record therefore
  // names no owner, and the worktree stays unrecorded.
  let launch_dir = world.profile_home.join("sessions/record-owner-launch/launches/20261113T000000-abcdef");
  fs::create_dir_all(&launch_dir).expect("the launch record directory");
  let record = serde_json::json!({
    "launch_id": "20261113T000000-abcdef",
    "cmd": "cargo test",
    "worktree_path": checkout.display().to_string(),
  });
  fs::write(launch_dir.join("metadata.json"), format!("{record:#}\n")).expect("the launch record");

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md\tcharliebot/launch-record", "dirty.md\tcharliebot/launch-record"] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list dropped the launch-record worktree path {listed}:\n{}",
      run.describe()
    );
  }
}

/// The path that a fake member CLI records when the round or a probe runs the member's copy of `kind` at `version`.
fn member_path(world: &World, kind: &str, version: &str) -> String {
  world.remote_home.join(format!("improve/runner/members/{kind}-{version}/bin/{kind}")).display().to_string()
}

#[test]
#[ignore = "integration"]
fn a_codex_round_runs_the_version_the_launcher_reads_not_the_hosts_copy() {
  let world = World::new();
  world.publish_member_cli("codex", "9.9.9", "codex-cli 9.9.9");
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut env = world.host_login_env();
  env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), "codex-cli 9.9.9".to_string());
  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.cli_runs(), [member_path(&world, "codex", "9.9.9")], "{}", run.describe());
  // Tool calls start the helpers beside the executable, so the install keeps the vendor's layout whole.
  let member = world.remote_home.join("improve/runner/members/codex-9.9.9");
  for helper in ["bin/codex-code-mode-host", "bin/codex-exec-server", "codex-path/rg", "codex-resources/bwrap"] {
    assert!(member.join(helper).is_file(), "the install lacks {helper}:\n{}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn a_codex_package_that_differs_from_its_published_digest_exits_7_and_installs_nothing() {
  let world = World::new();
  world.publish_member_cli("codex", "9.9.9", "codex-cli 9.9.9");
  // The package changes after the release listing names its digest, so the install must refuse what it fetched.
  let package = world.vendor_root.join(format!(
    "github.com/openai/codex/releases/download/rust-v9.9.9/codex-package-{}-unknown-linux-musl.tar.gz",
    std::env::consts::ARCH
  ));
  let mut bytes = fs::read(&package).expect("the fake package");
  bytes.push(b'x');
  fs::write(&package, bytes).expect("the altered package");
  let run = harness::run(&mut world.member_cli_command("codex", "9.9.9"));
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(stderr(&run).contains("the vendor publishes"), "{}", run.describe());
  assert!(stderr(&run).contains("improve host version none"), "{}", run.describe());
  let members = world.remote_home.join("improve/runner/members");
  assert!(!members.join("codex-9.9.9").exists(), "a package that failed its digest was installed");
  let stages: Vec<String> = fs::read_dir(&members)
    .map(|entries| {
      entries.map(|entry| entry.expect("a member entry").file_name().to_string_lossy().into_owned()).collect()
    })
    .unwrap_or_default();
  assert!(stages.iter().all(|name| !name.starts_with(".stage-")), "a failed install left a stage: {stages:?}");
}

#[test]
#[ignore = "integration"]
fn a_claude_round_runs_the_vendor_claude_at_the_launchers_version() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.cli_runs(), [member_path(&world, "claude", world::LOCAL_CLAUDE_VERSION)], "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_member_cli_the_vendor_does_not_publish_exits_7_and_starts_no_round() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut env = world.host_login_env();
  env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), "codex-cli 9.9.8".to_string());
  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));
  assert_refused_before_start(&world, &run, "9.9.8");
  assert!(stderr(&run).contains("improve host version none"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_member_cli_that_reports_another_version_exits_7_and_names_both() {
  let world = World::new();
  world.publish_member_cli("codex", "9.9.9", "codex-cli 9.9.8");
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut env = world.host_login_env();
  env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), "codex-cli 9.9.9".to_string());
  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));
  assert_refused_before_start(&world, &run, "9.9.9");
  assert!(stderr(&run).contains("improve host version codex-cli 9.9.8"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn an_unparseable_local_cli_version_exits_7_before_any_input_is_sent() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut env = world.host_login_env();
  env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), "codex-cli banana".to_string());
  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));
  assert_refused_before_input(&world, &run, "codex-cli banana");
}

#[test]
#[ignore = "integration"]
fn a_cached_member_cli_is_reused_without_a_download() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let env = world.host_login_env();
  let arguments = world.codex_arguments();
  let first = harness::run(&mut world.launcher_command_for(&env, &arguments));
  assert_eq!(first.code, 0, "{}", first.describe());
  // The release listing and the package: two fetches for the first install.
  let fetched = world.curl_calls();
  assert_eq!(fetched.len(), 2, "the first round fetched {fetched:?}");
  let second = harness::run(&mut world.launcher_command_for(&env, &arguments));
  assert_eq!(second.code, 0, "{}", second.describe());
  assert_eq!(world.curl_calls(), fetched, "the second round fetched again");
  let path = member_path(&world, "codex", world::LOCAL_CODEX_VERSION);
  assert_eq!(world.cli_runs(), [path.clone(), path]);
}

#[test]
#[ignore = "integration"]
fn concurrent_installs_of_one_member_cli_version_fetch_once_and_both_succeed() {
  let world = World::new();
  world.publish_member_cli("codex", "9.9.9", "codex-cli 9.9.9");
  let mut first = world.member_cli_command("codex", "9.9.9");
  first.env("FAKE_CURL_SLEEP", "1");
  let mut second = world.member_cli_command("codex", "9.9.9");
  second.env("FAKE_CURL_SLEEP", "1");
  let first = first.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("the first install starts");
  let second = second.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("the second install starts");
  let first = first.wait_with_output().expect("the first install ends");
  let second = second.wait_with_output().expect("the second install ends");
  assert!(first.status.success(), "the first install: {}", String::from_utf8_lossy(&first.stderr));
  assert!(second.status.success(), "the second install: {}", String::from_utf8_lossy(&second.stderr));
  assert_eq!(world.curl_calls().len(), 2, "two installs of one version fetched {:?}", world.curl_calls());
  assert!(Path::new(&member_path(&world, "codex", "9.9.9")).is_file(), "the installed CLI is missing");
}

#[test]
#[ignore = "integration"]
fn the_member_cache_keeps_the_live_and_last_used_versions_and_prunes_the_rest() {
  let world = World::new();
  let members = world.remote_home.join("improve/runner/members");
  for name in ["codex-0.100.0", "codex-0.101.0", "codex-0.102.0", ".stage-codex-0.90.0.abcdef"] {
    fs::create_dir_all(members.join(name).join("bin")).expect("a member directory");
  }
  std::os::unix::fs::symlink("codex-0.102.0", members.join(".current-codex")).expect("the current pointer");
  let stop = world.sandbox.root().join("stop-live-round");
  let mut live = world.spawn_live_runner(&stop, &["launch", "perf", "0123456789abcdef", "codex", "0.101.0"]);
  let run = harness::run(&mut world.member_cli_command("codex", world::LOCAL_CODEX_VERSION));
  fs::write(&stop, "").expect("the stop file");
  live.wait().expect("the live runner stand-in ends");
  assert_eq!(run.code, 0, "{}", run.describe());
  // The live round's version and the installed current version stay; the other two versions and the stage go.
  let mut kept: Vec<String> = fs::read_dir(&members)
    .expect("the member directory")
    .map(|entry| entry.expect("a member entry").file_name().to_string_lossy().into_owned())
    .filter(|name| !name.starts_with('.'))
    .collect();
  kept.sort();
  assert_eq!(kept, ["codex-0.101.0", "codex-0.167.0"], "{}", run.describe());
  assert!(!members.join(".stage-codex-0.90.0.abcdef").exists(), "a stage directory stayed");
}

#[test]
#[ignore = "integration"]
fn a_round_prunes_the_tools_of_commits_that_no_live_process_names() {
  let world = World::new();
  let bin = world.remote_home.join("improve/runner/bin");
  let live_commit = "1111111111111111111111111111111111111111";
  let dead_commit = "2222222222222222222222222222222222222222";
  for commit in [live_commit, dead_commit] {
    fs::create_dir_all(bin.join(commit)).expect("a commit's directory");
    for tool in ["charliebot", "improve-sandbox", "mock-home"] {
      fs::write(bin.join(format!("{tool}-{commit}")), "stale tool\n").expect("a commit's tool");
    }
  }
  let stop = world.sandbox.root().join("stop-live-round");
  let mut live =
    world.spawn_live_runner(&stop, &["launch", "perf", live_commit, "claude", world::LOCAL_CLAUDE_VERSION]);
  let run = world.run(&world.env());
  fs::write(&stop, "").expect("the stop file");
  live.wait().expect("the live runner stand-in ends");
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(bin.join(live_commit).is_dir(), "a live round's commit directory was pruned");
  assert!(bin.join(format!("charliebot-{live_commit}")).exists(), "a live round's tools were pruned");
  assert!(!bin.join(dead_commit).exists(), "a dead commit's directory stayed");
  assert!(!bin.join(format!("charliebot-{dead_commit}")).exists(), "a dead commit's tools stayed");
  assert!(bin.join(format!("charliebot-{}", world.commit)).exists(), "the round's own tools were pruned");
}
