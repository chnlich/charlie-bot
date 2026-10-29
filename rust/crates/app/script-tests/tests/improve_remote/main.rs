//! One improve round runs on the remote host: `tools/improve/remote-launch.sh` on this host and
//! `tools/improve/remote-runner.sh` there. A fake `ssh` records its arguments and runs the remote side locally, and
//! fake `claude`, `systemd-run` and `systemctl` record what they receive, so each test checks the round's contract
//! without a network, a server or a real remote host.
//!
//! The tests that assert a bound on time measure it: the approval preflight and the heartbeat stop must both finish
//! inside five seconds.

mod world;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use charliebot_shell_harness::{self as harness, Env};
use world::{DIRECTION, GATEWAY_TOKEN, World};

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
fn claudes_exit_code_passes_through() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_EXIT".to_string(), "7".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(world.claude_ran());
}

#[test]
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
}

#[test]
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
fn the_direction_comes_from_the_worker_session_and_its_task_file() {
  let world = World::new();
  // A task file of another session comes first in the directory; its direction must not win.
  fs::write(
    world.sandbox.dir("profile/config.d/cron.d").join("improve-ux.yaml"),
    "prompt_file: tools/improve/ux.md\nsession_id: 00000000-0000-0000-0000-000000000000\n",
  )
  .expect("another task file");
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(scope.contains("--unit improve-perf"), "the round ran in another direction's scope: {scope}");
  assert!(!scope.contains("--unit improve-ux"), "{scope}");
}

#[test]
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
fn a_worker_session_without_a_parent_exits_6() {
  let world = World::new();
  let metadata = world.profile_home.join("sessions").join(world::SESSION_ID).join("metadata.json");
  fs::write(&metadata, "{\n  \"id\": \"synthetic\"\n}\n").expect("the metadata without a parent");
  let run = world.run(&world.env());
  assert_eq!(run.code, 6, "{}", run.describe());
  assert!(!world.claude_ran());
}

#[test]
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
fn the_heartbeat_stops_the_scope_after_the_launcher_is_killed() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_WAIT_FOR".to_string(), world.scope_stopped.display().to_string());
  let mut command = world.launcher_command(&env);
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
fn the_other_directions_clone_adds_its_files_to_the_rounds_in_flight_list() {
  let world = World::new();
  // A standalone clone of another direction, like the host worker's five clones on the remote host.
  let other_root = world.remote_home.join("improve").join("ux");
  let clone = other_root.join("charlie-bot");
  fs::create_dir_all(&other_root).expect("the other direction root");
  world.git(&other_root, &["init", "-q", "-b", "main", "charlie-bot"]);
  world.git(&clone, &["remote", "add", "origin", "https://example.invalid/someone/charlie-bot.git"]);
  fs::write(clone.join("uncommitted.md"), "stale\n").expect("the tracked fixture file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic base"]);
  let base = world.git(&clone, &["rev-parse", "HEAD"]);
  world.git(&clone, &["update-ref", "refs/remotes/origin/main", &base]);
  // The committed change rides a topic branch, as a round's branch does.
  world.git(&clone, &["switch", "-q", "-c", "improve/ux/topic"]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed fixture file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic topic"]);
  // The uncommitted change sits in the checkout; the untracked file has no git entry at all.
  fs::write(clone.join("uncommitted.md"), "changed\n").expect("the uncommitted change");
  fs::write(clone.join("untracked.md"), "new\n").expect("the untracked file");

  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  for changed in ["committed.md", "uncommitted.md", "untracked.md"] {
    assert!(
      list.lines().any(|line| line == changed),
      "the in-flight list misses the {changed} change of the ux clone:\n{list}"
    );
  }
}

#[test]
fn the_boundary_denies_a_write_outside_the_allow_list_and_allows_one_inside() {
  let lsm = fs::read_to_string("/sys/kernel/security/lsm").unwrap_or_default();
  if !lsm.split(',').any(|name| name.trim() == "landlock") {
    println!("skipped: this kernel has no Landlock");
    return;
  }
  let sandbox = harness::Sandbox::new();
  let home = sandbox.dir("boundary-home");
  let canary = home.join("improve-canary-outside");
  fs::create_dir_all(&canary).expect("the canary directory");
  let script = harness::repo_root().join("tools/improve/landlock_exec.py");
  let mut env = Env::new();
  env.insert("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string());
  env.insert("HOME".to_string(), home.display().to_string());
  let mut command = harness::command("python3", &env);
  command
    .arg(&script)
    .arg(DIRECTION)
    .arg("--")
    .arg("bash")
    .arg("-c")
    .arg("printf inside >\"$HOME/improve/perf/evidence/inside.txt\"; printf outside >\"$HOME/improve-canary-outside/outside.txt\"");
  let run = harness::run(&mut command);
  assert_ne!(run.code, 0, "the write outside the allow-list succeeded: {}", run.describe());
  assert!(run.stderr.contains("Permission denied"), "{}", run.describe());
  assert!(home.join("improve/perf/evidence/inside.txt").is_file(), "the allowed write failed: {}", run.describe());
  assert!(!canary.join("outside.txt").exists(), "the denied write created a file");
}
