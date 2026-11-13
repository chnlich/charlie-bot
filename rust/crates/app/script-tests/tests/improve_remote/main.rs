//! One improve run: `dev/improve/remote-launch.sh` on this host, and `dev/improve/remote-runner.sh` on the
//! remote host for a round or this host's local claude for a cron session's own turn. A fake `ssh` records its
//! arguments and runs the remote side locally, and fake `claude`, `systemd-run`, `systemctl` and `cargo` record what
//! they receive, so each test checks the run's contract without a network, a server or a real remote host.
//!
//! The tests that assert a bound on time measure it: the approval preflight and the heartbeat stop must both finish
//! inside five seconds.

mod world;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Stdio;
use std::time::{Duration, Instant};

use charliebot_shell_harness as harness;
use sha2::{Digest as _, Sha256};
use world::{DIRECTION, GATEWAY_TOKEN, World};

/// The prompt the server writes to the launcher's standard input for a plain-text run.
const PROMPT: &str = "Improve one topic in the performance direction.\nReply with the word OK.\n";

fn stderr(run: &harness::Run) -> String {
  run.stderr.clone()
}

/// Waits for `condition` to hold, at most `limit`.
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
fn a_cron_sessions_own_turn_runs_this_hosts_claude_with_its_arguments_and_input() {
  let world = World::new();
  let prompt = world.sandbox.root().join("node-prompt.txt");
  fs::write(&prompt, PROMPT).expect("the prompt");
  let mut env = world.node_env();
  env.insert("FAKE_CLAUDE_STDIN".to_string(), world.node_claude_stdin.display().to_string());
  env.insert("FAKE_CLAUDE_EXIT".to_string(), "7".to_string());
  let stdin = fs::File::open(&prompt).expect("open the prompt");
  let mut command = world.launcher_command(&env);

  let run = harness::run_with_stdin(&mut command, Stdio::from(stdin));

  assert_eq!(run.code, 7, "the launcher did not exit with the local claude's code:\n{}", run.describe());
  assert!(world.ssh_arguments().is_empty(), "the node turn opened an ssh connection:\n{}", world.ssh_arguments());
  assert_eq!(world.node_claude_arguments(), world.claude_arguments());
  let received = fs::read_to_string(&world.node_claude_stdin).expect("the local claude recorded its standard input");
  assert_eq!(received, PROMPT, "the local claude did not receive the prompt:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_cron_sessions_own_turn_keeps_this_hosts_profile_home_config_dir_and_token() {
  let world = World::new();
  let run = world.run(&world.node_env());
  assert_eq!(run.code, 0, "{}", run.describe());

  let env = world.node_claude_environment();
  let value = |name: &str| env.get(name).map(String::as_str);
  let profile_home = world.profile_home.display().to_string();
  assert_eq!(value("CHARLIEBOT_HOME"), Some(profile_home.as_str()));
  assert_eq!(value("CLAUDE_CONFIG_DIR"), Some(world.local_config_dir));
  assert_eq!(value("CHARLIEBOT_RUN_TOKEN"), Some(world::RUN_TOKEN));
  let digest = format!("{:x}", Sha256::digest(GATEWAY_TOKEN));
  assert_eq!(value("ANTHROPIC_AUTH_TOKEN_SHA256"), Some(digest.as_str()));
}

#[test]
#[ignore = "integration"]
fn a_cron_sessions_own_turn_without_a_local_claude_exits_1_and_names_it() {
  let world = World::new();
  let mut env = world.node_env();
  // A PATH without the stub directory. The launcher's node branch needs only dirname, sed and head there.
  env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
  let mut probe = harness::command("bash", &env);
  probe.arg("-c").arg("type -P claude");
  let found = harness::run(&mut probe);
  assert!(
    found.stdout.trim().is_empty(),
    "/usr/bin or /bin holds a claude, so this PATH cannot test a missing one:\n{}",
    found.describe()
  );

  let run = world.run(&env);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("claude"), "the error names no missing CLI: {}", run.describe());
  assert!(world.ssh_arguments().is_empty(), "the node turn fell back to the remote host:\n{}", world.ssh_arguments());
  assert!(!world.node_claude_ran());
}

#[test]
#[ignore = "integration"]
fn a_round_worker_whose_parent_is_a_cron_session_still_runs_over_ssh() {
  let world = World::new();
  // The worker's metadata names PARENT_ID as its task_parent_id, and the cron task file binds
  // PARENT_ID, so this run is a round even though a cron session is one link away.
  let approval = world.sandbox.root().join("approval-needed");
  fs::write(&approval, "1\n").expect("the approval marker");
  let mut env = world.env();
  env.insert("SSH_APPROVAL_FILE".to_string(), approval.display().to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(world.ssh_arguments().contains("\ntrue\n"), "the preflight did not reach ssh:\n{}", world.ssh_arguments());
  assert!(!world.node_claude_ran(), "the round ran on this host");
}

#[test]
#[ignore = "integration"]
fn the_round_gets_the_gateway_token_under_both_tools_variables() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let env = world.claude_environment();
  let digest = format!("{:x}", Sha256::digest(GATEWAY_TOKEN));
  for name in ["ANTHROPIC_AUTH_TOKEN_SHA256", "CHARLIE_CODE_API_KEY_SHA256"] {
    assert_eq!(
      env.get(name).map(String::as_str),
      Some(digest.as_str()),
      "{name} does not carry the gateway token: {}",
      run.describe()
    );
  }
}

/// The stub improve-sandbox of the focused export checks: it records the digests of the two gateway variables it
/// receives, so the assertion holds no token value.
const SCOPE_ENV_STUB: &str = r#"# Fake improve-sandbox: record the digests of the two gateway variables, then exit.
printf 'ANTHROPIC_AUTH_TOKEN_SHA256=%s\nCHARLIE_CODE_API_KEY_SHA256=%s\n' \
  "$(printf '%s' "${ANTHROPIC_AUTH_TOKEN-}" | sha256sum | cut -d' ' -f1)" \
  "$(printf '%s' "${CHARLIE_CODE_API_KEY-}" | sha256sum | cut -d' ' -f1)" >"$FAKE_SCOPE_ENV"
exit 0
"#;

/// The direction of the focused export checks; its TMPDIR name no real direction's round shares.
const FOCUS_DIRECTION: &str = "focused";

/// Runs the runner's `--in-scope` form alone, with the stub improve-sandbox that records the gateway variables.
fn run_in_scope(world: &World, launch_id: &str) -> harness::Run {
  let launch = world.remote_home.join("improve/launch").join(launch_id);
  fs::create_dir_all(&launch).expect("the launch directory");
  fs::write(launch.join("args"), "").expect("the launch arguments file");
  fs::create_dir_all(format!("/tmp/improve-{FOCUS_DIRECTION}-tmp")).expect("the round's TMPDIR");
  fs::create_dir_all(world.remote_home.join("improve").join(FOCUS_DIRECTION)).expect("the round root");
  let bin = world.remote_home.join("improve/runner/bin");
  fs::create_dir_all(&bin).expect("the runner bin directory");
  harness::write_executable(&bin.join("improve-sandbox"), format!("#!/usr/bin/env bash\n{SCOPE_ENV_STUB}"));
  let mut env = harness::Env::new();
  env.insert("HOME".to_string(), world.remote_home.display().to_string());
  env.insert("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string());
  env.insert("FAKE_SCOPE_ENV".to_string(), world.sandbox.root().join("scope-env.txt").display().to_string());
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("dev/improve/remote-runner.sh")).args([
    "--in-scope",
    launch_id,
    FOCUS_DIRECTION,
  ]);
  harness::run(&mut command)
}

#[test]
#[ignore = "integration"]
fn the_in_scope_runner_exports_the_credentials_token_under_both_tools_names() {
  let world = World::new();
  // The fixture token differs from the launcher's, so the check reads the credentials file and not the environment.
  let token = "focused-gateway-token";
  fs::write(
    world.remote_home.join("improve/home/credentials.yaml"),
    format!("synthetic-gateway:\n  api_key: {token}\n"),
  )
  .expect("the fixture credentials file");

  let run = run_in_scope(&world, "20261101T090000Z-1-300");

  assert_eq!(run.code, 0, "{}", run.describe());
  let record = world.sandbox.root().join("scope-env.txt");
  let text = fs::read_to_string(&record).expect("the stub improve-sandbox recorded its environment");
  let digest = format!("{:x}", Sha256::digest(token));
  for name in ["ANTHROPIC_AUTH_TOKEN_SHA256", "CHARLIE_CODE_API_KEY_SHA256"] {
    assert!(text.contains(&format!("{name}={digest}\n")), "{name} misses the fixture token:\n{text}");
  }
}

#[test]
#[ignore = "integration"]
fn the_in_scope_runner_without_a_credentials_token_exits_1_and_names_the_file() {
  let world = World::new();
  fs::write(world.remote_home.join("improve/home/credentials.yaml"), "").expect("the empty credentials file");
  let credentials = world.remote_home.join("improve/home/credentials.yaml");

  let run = run_in_scope(&world, "20261101T090100Z-1-301");

  assert_eq!(run.code, 1, "{}", run.describe());
  let message = stderr(&run);
  assert!(
    message.contains("the round's credentials file holds no gateway token")
      && message.contains(credentials.display().to_string().as_str()),
    "the failure does not name the tokenless credentials file: {message}"
  );
}

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
fn no_token_value_reaches_an_ssh_argument_or_any_file_but_the_credentials_file() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let arguments = world.ssh_arguments();
  assert!(!arguments.is_empty(), "the fake ssh recorded no call");
  for token in [GATEWAY_TOKEN, world::API_KEY, world::RUN_TOKEN] {
    assert!(!arguments.contains(token), "a token value reached an ssh argument:\n{arguments}");
  }

  let credentials = world.remote_home.join("improve/home/credentials.yaml");
  let text = fs::read_to_string(&credentials).expect("the runner wrote the credentials file");
  assert!(text.contains(GATEWAY_TOKEN), "the gateway token is absent from {credentials:?}: {text}");
  let mode = fs::metadata(&credentials).expect("credentials metadata").permissions().mode() & 0o777;
  assert_eq!(mode, 0o600, "{credentials:?} has mode {mode:o}");
  for path in world.tree(&world.remote_home) {
    let file = world.remote_home.join(&path);
    if file == credentials || !file.is_file() {
      continue;
    }
    let contents = fs::read_to_string(&file).unwrap_or_default();
    assert!(!contents.contains(GATEWAY_TOKEN), "a token reached {path}");
    assert!(!contents.contains(world::API_KEY), "a token reached {path}");
  }
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
  assert_eq!(value("TMPDIR"), Some("/tmp/improve-perf-tmp"));
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
  // A task file of another session comes first in the directory; its direction must not win.
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
  assert!(!world.node_claude_ran(), "a session that no cron task file names ran on this host");
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
  // A launch directory alone is not a live round: its wrapper removes it when it exits, and the
  // leader that would name it is gone, so the scope's round is over and only a leftover process
  // keeps the unit loaded.
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

#[test]
#[ignore = "integration"]
fn the_other_directions_clone_adds_its_files_to_the_rounds_in_flight_list() {
  let world = World::new();
  // A standalone clone of another direction, like the host worker's five clones on the remote host.
  let clone = world.other_direction_clone("ux");
  // A file whose removal main lands after the topic branch's base, as a rename's old name does.
  fs::write(clone.join("dropped.md"), "old\n").expect("the dropped fixture file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "add the dropped file"]);
  // The committed change rides a topic branch, as a round's branch does, and it touches the old
  // path beside the live one.
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
  // The uncommitted change sits in the checkout; the untracked file has no git entry at all.
  fs::write(clone.join("base.md"), "changed\n").expect("the uncommitted change");
  fs::write(clone.join("dropped.md"), "uncommitted\n").expect("the dropped path's uncommitted change");
  fs::write(clone.join("untracked.md"), "new\n").expect("the untracked file");

  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  for changed in ["committed.md", "base.md", "dropped.md", "untracked.md"] {
    assert!(
      list.lines().any(|line| line == changed),
      "the in-flight list misses the {changed} change of the ux clone:\n{list}"
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
    list.lines().any(|line| line == "pr-only.md"),
    "the in-flight list misses an open pull request's file:\n{list}"
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
    list.lines().any(|line| line == "shared.md"),
    "the in-flight list misses an unlanded branch's file that origin/main changed:\n{list}"
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
  assert_eq!(
    listed,
    ["base.md", "landed.md", "together.md", "unlanded.md"],
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
  assert!(list.lines().any(|line| line == "rebased.md"), "the unowned worktree drops its committed path:\n{list}");
}

#[test]
#[ignore = "integration"]
fn an_unowned_other_directions_clone_keeps_a_landed_branch_path() {
  let world = World::new();
  let clone = world.other_direction_clone("ux");
  // The topic lands on the remote's main as a squash merge, but no task names the clone.
  world.git(&clone, &["switch", "-q", "-c", "improve/ux/stale"]);
  fs::write(clone.join("stale.md"), "landed\n").expect("the landed topic's file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic stale topic"]);
  world.git(&clone, &["push", "-q", "origin", "improve/ux/stale"]);
  let landing = world.sandbox.dir("landing/ux");
  let origin = world.other_direction_origin("ux").display().to_string();
  world.git(&world.sandbox.dir("landing"), &["clone", "-q", &origin, "ux"]);
  world.git(&landing, &["merge", "-q", "--squash", "origin/improve/ux/stale"]);
  world.git(&landing, &["commit", "-q", "-m", "land the stale topic"]);
  world.git(&landing, &["push", "-q", "origin", "main"]);
  // The clone's origin/main still points behind the merge, as a stale checkout's does.
  let behind = world.git(&clone, &["rev-parse", "HEAD~1"]);
  world.git(&clone, &["update-ref", "refs/remotes/origin/main", &behind]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(
    list.lines().any(|line| line == "stale.md"),
    "the unowned clone drops a committed path after main lands it:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn an_unowned_other_directions_branch_keeps_all_changed_paths() {
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

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  let mut listed: Vec<&str> = list.lines().collect();
  listed.sort_unstable();
  assert_eq!(listed, ["base.md", "landed.md", "unlanded.md"], "the unowned clone loses a changed path:\n{list}");
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
  // The commit the runner builds needs a rust/ directory, and its origin is a bare copy of the launcher's repository.
  fs::create_dir_all(world.repo.join("rust")).expect("the rust directory");
  fs::write(world.repo.join("rust/Cargo.toml"), "[workspace]\n").expect("the workspace file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "synthetic build commit"]);
  let commit = world.git(&world.repo, &["rev-parse", "HEAD"]);
  let origin = world.sandbox.root().join("origin.git");
  let origin_text = origin.display().to_string();
  world.git(&world.repo, &["clone", "-q", "--bare", ".", &origin_text]);
  world.git(&world.repo, &["remote", "set-url", "origin", &origin_text]);
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
    "synthetic-gateway:\n  api_key: synthetic-token\n",
  )
  .expect("the round's credentials file");
  // The round home keeps the pattern file of the previous round, as the host's does, so both
  // runners' copies take the in-place path instead of racing on a fresh file.
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
    let launch = world.remote_home.join("improve/launch").join(&launch_id);
    fs::create_dir_all(&launch).expect("the launch directory");
    harness::copy_file(&harness::repo_root().join("dev/improve/remote-runner.sh"), &launch.join("remote-runner.sh"));
    // The launcher sends the rule script beside the runner, so the runner's clone scan sources it.
    harness::copy_file(&harness::repo_root().join("dev/improve/inflight-files.sh"), &launch.join("inflight-files.sh"));
    for name in ["args", "env", "paths"] {
      fs::write(launch.join(name), "").expect(name);
    }
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
    // The build runs long enough that the second runner arrives while the first one holds the
    // work, as two burst starts do on the host.
    env.insert("FAKE_CARGO_SLEEP".to_string(), "2".to_string());
    env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
    env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
    let mut command = harness::command("bash", &env);
    command.arg(launch.join("remote-runner.sh")).args([&launch_id, direction, &commit]);
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
    let commit_bin = runner_bin.join(&commit).display().to_string();
    assert!(env.contains(&format!("PATH={commit_bin}:")), "the {direction} round did not pin its tools:\n{env}");
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
  // The uncommitted change sits in the checkout; the untracked file has no git entry at all.
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
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  let mut listed: Vec<&str> = list.lines().collect();
  listed.sort_unstable();
  assert_eq!(
    listed,
    ["base.md", "committed.md", "untracked.md"],
    "the in-flight list is not the ux clone's files:\n{list}"
  );
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
  for listed in ["locked.md", "dirty.md"] {
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
  let mut listed: Vec<&str> = run.stdout.lines().collect();
  listed.sort_unstable();
  assert_eq!(
    listed,
    ["fresh.md", "live.md", "old.md", "pr-only.md"],
    "the in-flight list drops a path from an unowned worktree:\n{}",
    run.describe()
  );
}

/// Writes one task lifecycle state to the profile home's task store.
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

/// Writes a run record that links one task to its worktree. Its end time does not set the task state.
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

/// Runs the in-flight list out of its checkout, as the launcher does.
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
  for listed in ["topic.md", "dirty.md"] {
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
      !run.stdout.lines().any(|line| line == listed),
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
      !run.stdout.lines().any(|line| line == listed),
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
  for listed in ["topic.md", "dirty.md", "pr-only.md"] {
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
  for listed in ["topic.md", "staged.md", "untracked.md"] {
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
  for listed in ["topic.md", "dirty.md"] {
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
