//! The switch command's tests (scripts/switch-server.sh), run for real against stub commands and the stand-in servers.
//!
//! These are the tests of tests/test_switch_server.py, with every parametrized case as its own test. Each asserts the
//! output lines and the exit code of its original: the main module holds the preflight, readback, escalation and pane
//! tests, `probe` the probe-turn tests, and `delegation` the delegation-check tests and the restore they drive.

mod cleanup;
mod delegation;
mod probe;
mod world;

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use charliebot_core::logline;
use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use regex::Regex;
use serde_json::{Value, json};
use wait_timeout::ChildExt;
use world::{
  SESSION, World, fake_charliebot, fake_pane_server, install_release, pid_start, quote, replace_charliebot,
  repo_switch, sha1_hex, step_target_sha, stop, wait_until, write_behavior,
};

// ---- home helper: the config's port -------------------------------------------------------------------------------

#[test]
fn test_home_helper_config_uses_server_default_host_only() {
  home_helper_config_uses_server_default("server:\n  host: 127.0.0.1\n");
}

#[test]
fn test_home_helper_config_uses_server_default_absent() {
  home_helper_config_uses_server_default("{}\n");
}

/// A config without a port reads the default port.
fn home_helper_config_uses_server_default(config: &str) {
  let world = World::new();
  fs::write(world.home.join("config.yaml"), config).expect("the config");

  let run = world.home_helper("config");

  // Owner: default_port of ServerConfig in rust/crates/infra/core/src/config.rs.
  let rust_config =
    fs::read_to_string(harness::repo_root().join("rust/crates/infra/core/src/config.rs")).expect("the config source");
  let pattern = Regex::new(r"fn default_port\(\) -> u16 \{\n\s*(\d+)").expect("the pattern");
  let default_port: u16 = pattern.captures(&rust_config).expect("default_port")[1].parse().expect("a port");
  assert_eq!(first_line_port(&run), default_port, "{}", world.report(&run));
}

#[test]
fn test_home_helper_config_uses_explicit_port() {
  let world = World::new();

  let run = world.home_helper("config");

  assert_eq!(first_line_port(&run), world.port, "{}", world.report(&run));
}

fn first_line_port(run: &Run) -> u16 {
  run.stdout.lines().next().expect("the port line").parse().expect("a port")
}

// ---- live backend switch directions ---------------------------------------------------------------------------------

#[test]
fn test_live_backend_switch_directions_both_carry_agy_gemini_flash() {
  live_backend_switch_directions(true, true, "agy-gemini-flash");
}

#[test]
fn test_live_backend_switch_directions_both_carry_oc_deepseek_flash() {
  live_backend_switch_directions(true, true, "oc-deepseek-flash");
}

#[test]
fn test_live_backend_switch_directions_both_carry_claude_opus() {
  live_backend_switch_directions(true, true, "claude-opus");
}

#[test]
fn test_live_backend_switch_directions_both_carry_oc_retired() {
  live_backend_switch_directions(true, true, "oc-retired");
}

#[test]
fn test_live_backend_switch_directions_running_lacks_agy_gemini_flash() {
  live_backend_switch_directions(false, true, "agy-gemini-flash");
}

#[test]
fn test_live_backend_switch_directions_running_lacks_oc_deepseek_flash() {
  live_backend_switch_directions(false, true, "oc-deepseek-flash");
}

#[test]
fn test_live_backend_switch_directions_running_lacks_claude_opus() {
  live_backend_switch_directions(false, true, "claude-opus");
}

#[test]
fn test_live_backend_switch_directions_running_lacks_oc_retired() {
  live_backend_switch_directions(false, true, "oc-retired");
}

#[test]
fn test_live_backend_switch_directions_target_lacks_agy_gemini_flash() {
  live_backend_switch_directions(true, false, "agy-gemini-flash");
}

#[test]
fn test_live_backend_switch_directions_target_lacks_oc_deepseek_flash() {
  live_backend_switch_directions(true, false, "oc-deepseek-flash");
}

#[test]
fn test_live_backend_switch_directions_target_lacks_claude_opus() {
  live_backend_switch_directions(true, false, "claude-opus");
}

#[test]
fn test_live_backend_switch_directions_target_lacks_oc_retired() {
  live_backend_switch_directions(true, false, "oc-retired");
}

/// Rust builds read each backend's re-attachment declaration from their own source tree, in either layout. The running
/// and target builds each carry or lack the declaration; a switch reads with the carrying builds only, except that
/// Claude Code re-attaches in every direction.
fn live_backend_switch_directions(running_carries: bool, target_carries: bool, backend: &str) {
  let mut world = World::new();
  world.seed_live_run("run-direction", Some(backend));
  let running = if running_carries { world.rust_sha.clone() } else { world.old_sha.clone() };
  let target = if target_carries { world.rust_sha.clone() } else { world.old_sha.clone() };

  let run = world.stubbed_preflight(&format!("rust {running}"), "rust", &target);

  let backend_type = match backend {
    "agy-gemini-flash" => Some("antigravity"),
    "oc-deepseek-flash" => Some("opencode"),
    "claude-opus" => Some("cc-claude"),
    _ => None,
  };
  let allowed = backend_type.is_some() && (backend_type == Some("cc-claude") || (running_carries && target_carries));
  assert_eq!(run.code, if allowed { 0 } else { 1 }, "{}", world.report(&run));
  if !allowed {
    assert!(run.stdout.contains(backend), "{}", world.report(&run));
    assert!(run.stdout.contains("run-direction"), "{}", world.report(&run));
    if backend != "oc-retired" {
      assert!(run.stdout.contains("cannot be re-attached after a restart"), "{}", world.report(&run));
      let lacking = if running_carries { "target" } else { "running" };
      let marker = format!("{lacking} build {} marks restart_reattach = false", world.old_sha);
      assert!(run.stdout.contains(&marker), "{}", world.report(&run));
    }
  }
  if backend == "oc-retired" {
    assert!(run.stdout.contains("restart re-attachment is unknown"), "{}", world.report(&run));
  }
  world.assert_live_running();
}

// ---- backend registration layouts -----------------------------------------------------------------------------------

#[test]
fn test_backend_registration_reads_the_layout_of_each_build_old_layout_to_new_layout() {
  backend_registration_reads_the_layout_of_each_build(
    "old",
    "rust",
    "running",
    "rust/crates/runtime/agents/src/backends/antigravity/mod.rs",
  );
}

#[test]
fn test_backend_registration_reads_the_layout_of_each_build_new_layout_to_new_layout() {
  backend_registration_reads_the_layout_of_each_build(
    "rust",
    "new",
    "target",
    "rust/crates/backends/antigravity/src/lib.rs",
  );
}

#[test]
fn test_backend_registration_reads_the_layout_of_each_build_new_layout_back_to_old_layout() {
  backend_registration_reads_the_layout_of_each_build(
    "new",
    "old",
    "target",
    "rust/crates/runtime/agents/src/backends/antigravity/mod.rs",
  );
}

#[test]
fn test_backend_registration_reads_the_layout_of_each_build_pre_grouping_to_split() {
  backend_registration_reads_the_layout_of_each_build(
    "pre",
    "rust",
    "running",
    "rust/crates/charliebot-agents/src/backends/antigravity/mod.rs",
  );
}

#[test]
fn test_backend_registration_reads_the_layout_of_each_build_split_back_to_pre_grouping() {
  backend_registration_reads_the_layout_of_each_build(
    "rust",
    "pre",
    "target",
    "rust/crates/charliebot-agents/src/backends/antigravity/mod.rs",
  );
}

/// A switch between builds reads each build's registration where that build's tree keeps it: the backend's own crate
/// after the split, the runtime/agents module before it, the agents crate before the grouping.
fn backend_registration_reads_the_layout_of_each_build(running: &str, target: &str, reported: &str, detail: &str) {
  let mut world = World::new();
  world.seed_live_run("run-layout", Some("agy-gemini-flash"));

  let run = world.stubbed_preflight(&format!("rust {}", build_sha(&world, running)), "rust", build_sha(&world, target));

  assert_eq!(run.code, 1, "{}", world.report(&run));
  let sha = if reported == "running" { build_sha(&world, running) } else { build_sha(&world, target) };
  let marker = format!("{reported} build {sha} marks restart_reattach = false ({detail})");
  assert!(run.stdout.contains(&marker), "{}", world.report(&run));
  world.assert_live_running();
}

fn build_sha<'a>(world: &'a World, name: &str) -> &'a str {
  match name {
    "pre" => &world.pre_sha,
    "old" => &world.old_sha,
    "rust" => &world.rust_sha,
    "new" => &world.new_sha,
    other => panic!("no build named {other}"),
  }
}

// ---- preflight: a live run from the build before the grouping -------------------------------------------------------

#[test]
fn test_pre_grouping_build_passes_the_preflight_with_a_live_run_grouped_layout() {
  pre_grouping_build_passes_the_preflight_with_a_live_run("old");
}

#[test]
fn test_pre_grouping_build_passes_the_preflight_with_a_live_run_split_layout() {
  pre_grouping_build_passes_the_preflight_with_a_live_run("rust");
}

/// A live switch from the build before the crate grouping passes: the preflight finds the live run's re-attachable
/// backend type in the agents crate registration, then in the target build's own, grouped or split per backend.
fn pre_grouping_build_passes_the_preflight_with_a_live_run(target: &str) {
  let mut world = World::new();
  world.seed_live_run("run-pre-grouping", Some("claude-opus"));

  let run = world.stubbed_preflight(&format!("rust {}", world.pre_sha), "rust", build_sha(&world, target));

  assert_eq!(run.code, 0, "{}", world.report(&run));
  let passed = format!("preflight passed: rust {}", world.pre_sha);
  assert!(run.stdout.contains(&passed), "{}", world.report(&run));
  let runs = fs::read_to_string(world.root().join("preflight.runs")).expect("the runs file");
  assert!(runs.contains("run-pre-grouping"), "{}", world.report(&run));
  world.assert_live_running();
}

// ---- preflight: a build missing from the clone ----------------------------------------------------------------------

#[test]
fn test_preflight_fails_when_a_build_is_not_in_the_clone_running() {
  preflight_fails_when_a_build_is_not_in_the_clone("running");
}

#[test]
fn test_preflight_fails_when_a_build_is_not_in_the_clone_target() {
  preflight_fails_when_a_build_is_not_in_the_clone("target");
}

/// The switch refuses a live run when one build has no readable backend registration.
fn preflight_fails_when_a_build_is_not_in_the_clone(missing: &str) {
  let mut world = World::new();
  let absent = sha1_hex(b"a build no clone has");
  world.seed_live_run("run-missing-build", Some("oc-deepseek-flash"));
  let (running_impl, target_sha) = if missing == "target" {
    install_release(&world.releases, &absent);
    (format!("rust {}", world.rust_sha), absent.clone())
  } else {
    (format!("rust {absent}"), world.rust_sha.clone())
  };

  let run = world.stubbed_preflight(&running_impl, "rust", &target_sha);

  assert_eq!(run.code, 1, "{}", world.report(&run));
  if missing == "target" {
    let refused = format!("target commit {absent} is not in the runtime clone");
    assert!(run.stdout.contains(&refused), "{}", world.report(&run));
  } else {
    assert!(run.stdout.contains(&format!("running build {absent}")), "{}", world.report(&run));
    assert!(run.stdout.contains("cannot inspect build"), "{}", world.report(&run));
  }
}

// ---- preflight refusals: exit 1 and no change -----------------------------------------------------------------------

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_checksum() {
  preflight_refusal_exits_1_and_changes_nothing("checksum");
}

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_check_config() {
  preflight_refusal_exits_1_and_changes_nothing("check-config");
}

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_opencode_run() {
  preflight_refusal_exits_1_and_changes_nothing("opencode-run");
}

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_antigravity_run() {
  preflight_refusal_exits_1_and_changes_nothing("antigravity-run");
}

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_unknown_backend_run() {
  preflight_refusal_exits_1_and_changes_nothing("unknown-backend-run");
}

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_missing_backend_run() {
  preflight_refusal_exits_1_and_changes_nothing("missing-backend-run");
}

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_improve_loop() {
  preflight_refusal_exits_1_and_changes_nothing("improve-loop");
}

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_dirty_clone() {
  preflight_refusal_exits_1_and_changes_nothing("dirty-clone");
}

#[test]
fn test_preflight_refusal_exits_1_and_changes_nothing_launcher_preflight() {
  preflight_refusal_exits_1_and_changes_nothing("launcher-preflight");
}

/// Each preflight check refuses before the switch stops anything: exit 1 and no change. The refusals that a live run
/// raises run against a started release, because the scan reads the running build's registrations.
fn preflight_refusal_exits_1_and_changes_nothing(case: &str) {
  let mut world = World::new();
  let release = world.releases.join(&world.rust_sha);
  let backend = match case {
    "opencode-run" => Some("oc-deepseek-flash"),
    "antigravity-run" => Some("agy-gemini-flash"),
    "unknown-backend-run" => Some("oc-retired"),
    _ => None,
  };
  match case {
    "checksum" => {
      let path = release.join("charliebot-server");
      let mut bytes = fs::read(&path).expect("the release binary");
      bytes.extend_from_slice(b"# tampered\n");
      fs::write(&path, bytes).expect("tamper with the release binary");
    }
    "check-config" => write_behavior(&release, &[("check_config", json!("fail"))]),
    "opencode-run" | "antigravity-run" | "unknown-backend-run" | "missing-backend-run" => {
      world.seed_live_run("run-blocked", backend);
      world.start_release();
    }
    "improve-loop" => {
      let controller = world.spawn_live();
      seed_improve_loop(&world, controller);
      world.start_release();
    }
    "dirty-clone" => fs::write(world.clone.join("scripts/start-server.sh"), "# local edit\n").expect("the local edit"),
    "launcher-preflight" => {
      world.env.insert("SYSTEMD_RUN_REFUSE_ALL".to_string(), "1".to_string());
      world.start_release();
    }
    other => panic!("no preflight case named {other}"),
  }

  let home_before = world.home_snapshot();
  let current_before = readlink_opt(&world.releases.join("current"));
  let rust_sha = world.rust_sha.clone();

  let run = world.switch(&["rust", &rust_sha]);

  assert_eq!(run.code, 1, "{}", world.report(&run));
  assert_eq!(readlink_opt(&world.releases.join("current")), current_before, "{}", world.report(&run));
  assert_eq!(world.git(&world.clone, &["rev-parse", "HEAD"]), world.old_sha, "{}", world.report(&run));
  assert_eq!(world.home_snapshot(), home_before, "{}", world.report(&run));
  world.assert_live_running();
  if case == "launcher-preflight" {
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(output.contains("launcher preflight"), "{}", world.report(&run));
  }
  if case.ends_with("-run") {
    assert!(run.stdout.contains("a restart would lose live work:"), "{}", world.report(&run));
    let lines: Vec<String> = world.home_helper("scan").stdout.lines().map(String::from).collect();
    let live_pid = world.live[0].id();
    let backend_name = backend.unwrap_or("None");
    let live: Vec<&String> = lines.iter().filter(|line| line.starts_with("live\t")).collect();
    let expected_live = format!("live\t{SESSION}\trun-blocked\t{backend_name}\t{live_pid}\t{}", pid_start(live_pid));
    assert_eq!(live, vec![&expected_live]);
    let blocks: Vec<&String> = lines.iter().filter(|line| line.starts_with("block\t")).collect();
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert!(blocks[0].contains("run-blocked"), "{}", blocks[0]);
    assert!(blocks[0].contains(backend_name), "{}", blocks[0]);
    if case == "opencode-run" || case == "antigravity-run" {
      assert!(blocks[0].contains(case.strip_suffix("-run").expect("a -run suffix")), "{}", blocks[0]);
      assert!(blocks[0].contains(&live_pid.to_string()), "{}", blocks[0]);
    }
  }
}

/// An improve loop marked running under a live controller process.
fn seed_improve_loop(world: &World, controller_pid: u32) {
  let loop_dir = world.home.join("sessions").join(SESSION).join("loops").join("1");
  fs::create_dir_all(&loop_dir).expect("the loop directory");
  let state = json!({"status": "running", "server_pid": controller_pid});
  fs::write(loop_dir.join("state.json"), state.to_string()).expect("the loop state");
}

/// The target of `path` when it is a symlink, as `os.readlink` reads it.
fn readlink_opt(path: &Path) -> Option<PathBuf> {
  path.is_symlink().then(|| fs::read_link(path).expect("read the link"))
}

// ---- the readback's adoption check ----------------------------------------------------------------------------------

/// The launch output the adoption check reads: the drain line the new server writes for a run whose process had ended.
/// Its resume line is the one the check wants instead for a run whose process still runs.
fn drain_line(run_id: &str) -> String {
  let fields = [("session".to_string(), json!(SESSION)), ("run_id".to_string(), json!(run_id))];
  logline::render("2026-10-26 02:15:04.200Z", "warning", "task_recovery_drain", &fields)
}

#[test]
fn test_adoption_passes_when_the_recorded_run_ended_during_the_switch() {
  let mut world = World::new();
  let run_id = "run-ended";
  world.seed_live_run(run_id, Some("claude-opus"));
  let run_path = world.home.join("sessions").join(SESSION).join("data/runs").join(run_id).join("metadata.json");
  let mut run: Value = serde_json::from_str(&fs::read_to_string(&run_path).expect("the run record")).expect("a record");
  run["kind"] = json!("manager_turn");
  fs::write(&run_path, run.to_string()).expect("the manager-turn record");
  let events_path = world.home.join("sessions").join(SESSION).join("data/chat_events.jsonl");
  OpenOptions::new().create(true).append(true).open(&events_path).expect("the chat events file");
  // Recorded while the process still ran, as the preflight scan does.
  let runs_file = world.scanned_runs_file();
  world.live[0].kill().expect("kill the run's process");
  world.live[0].wait().expect("reap the run's process");

  let (done_tx, done_rx) = mpsc::channel();
  let home = world.home.clone();
  thread::spawn(move || {
    thread::sleep(Duration::from_millis(200));
    world::record_outcome(&home, run_id, "success");
    done_tx.send(()).expect("the test waits for the writer");
  });
  let run = world.adopted(&runs_file, &drain_line(run_id));

  assert!(done_rx.recv_timeout(Duration::from_secs(1)).is_ok(), "the outcome writer did not finish");
  assert_eq!(run.code, 0, "{}", run.describe());
  let outcome = format!("readback: run {run_id} ended during the switch: outcome success");
  assert!(run.stdout.contains(&outcome), "{}", run.describe());
  assert!(!run.stdout.contains("was not adopted"), "{}", run.describe());
}

#[test]
fn test_adoption_fails_only_for_a_recorded_run_that_still_runs() {
  let mut world = World::new();
  world.seed_live_run("run-ended", Some("claude-opus"));
  let ended_pid = world.live[0].id();
  world.seed_live_run("run-live", Some("claude-opus"));
  let live_pid = world.live[1].id();
  let runs_file = world.runs_file(&[
    format!("live\t{SESSION}\trun-ended\tclaude-opus\t{ended_pid}\t{}", pid_start(ended_pid)),
    format!("live\t{SESSION}\trun-live\tclaude-opus\t{live_pid}\t{}", pid_start(live_pid)),
  ]);
  world.live[0].kill().expect("kill the ended run's process");
  world.live[0].wait().expect("reap the ended run's process");
  world.record_outcome("run-ended", "success");
  let events = world.home.join("sessions").join(SESSION).join("data/chat_events.jsonl");
  let archive = world.home.join("sessions").join(SESSION).join("data/archives/chat_events.2026-W01.jsonl");
  fs::create_dir_all(archive.parent().expect("the archive directory")).expect("the archive directory");
  fs::rename(&events, &archive).expect("archive the chat events");

  let run = world.adopted(&runs_file, "");

  assert_eq!(run.code, 1, "{}", run.describe());
  let ended = "readback: run run-ended ended during the switch: outcome success";
  assert!(run.stdout.contains(ended), "{}", run.describe());
  assert!(run.stdout.contains("readback: run run-live was not adopted"), "{}", run.describe());
}

#[test]
fn test_adoption_treats_a_reused_pid_as_an_ended_run() {
  let mut world = World::new();
  world.seed_live_run("run-reused", Some("claude-opus"));
  let pid = world.live[0].id();
  // This pid now belongs to a process with another start time, so the recorded run ended.
  let start: u64 = pid_start(pid).parse().expect("a start time");
  let runs_file = world.runs_file(&[format!("live\t{SESSION}\trun-reused\tclaude-opus\t{pid}\t{}", start + 1)]);
  world.record_outcome("run-reused", "success");

  let run = world.adopted(&runs_file, "");

  assert_eq!(run.code, 0, "{}", run.describe());
  let ended = "readback: run run-reused ended during the switch: outcome success";
  assert!(run.stdout.contains(ended), "{}", run.describe());
}

// ---- the forward path's exit by the outcome of a stopped run --------------------------------------------------------

#[test]
fn test_switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended_completed() {
  switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended(Some("success"));
}

#[test]
fn test_switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended_interrupted() {
  switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended(Some("interrupted"));
}

#[test]
fn test_switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended_failed() {
  switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended(Some("failed"));
}

#[test]
fn test_switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended_no_outcome() {
  switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended(None);
}

/// A run the stop ended passes the switch only on a recorded completion. Any other outcome keeps the new server, exits
/// 4 and restores nothing; a run with no outcome counts as not completed.
fn switch_exits_by_the_recorded_outcome_of_a_run_the_stop_ended(outcome: Option<&str>) {
  let mut world = World::new();
  world.seed_live_run("run-stopped", Some("claude-opus"));
  if let Some(outcome) = outcome {
    world.record_outcome("run-stopped", outcome);
  }
  let home_before = world.home_snapshot();
  let pid = world.live[0].id();

  let run = world.switch_forward(pid, &drain_line("run-stopped"), ":");

  assert!(!world.root().join("restored").exists(), "{}", world.report(&run));
  assert_eq!(world.home_snapshot(), home_before, "{}", world.report(&run));
  if outcome == Some("success") {
    assert_eq!(run.code, 0, "{}", world.report(&run));
    let completed = "readback: run run-stopped ended during the switch: outcome success";
    assert!(run.stdout.contains(completed), "{}", world.report(&run));
    assert!(run.stdout.contains("SWITCH COMPLETE:"), "{}", world.report(&run));
  } else {
    assert_eq!(run.code, 4, "{}", world.report(&run));
    let died =
      format!("readback: run run-stopped died during the switch: outcome {}", outcome.unwrap_or("none recorded"));
    assert!(run.stdout.contains(&died), "{}", world.report(&run));
    assert!(
      run.stdout.contains("SWITCH COMPLETE, BUT A RECORDED RUN DIED DURING THE SWITCH"),
      "{}",
      world.report(&run)
    );
  }
}

// ---- escalation: a server under the pane is stopped by SIGTERM, then SIGKILL --------------------------------------

#[test]
fn test_escalation_stops_a_server_under_the_pane_with_sigterm() {
  let sandbox = Sandbox::new();
  let releases = sandbox.dir("releases");
  let mut pane = Pane::start(&sandbox, &releases, "honest");
  let server = pane.server_under();

  let run = escalate(&sandbox, &releases, pane.pid());

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(logged_with(&run.stdout, "SIGTERM", server), "{}", run.describe());
  assert!(!run.stdout.contains("SIGKILL"), "{}", run.describe());
  assert_eq!(pane.wait_exit(Duration::from_secs(5)), 0);
}

#[test]
fn test_escalation_sigkills_a_survivor_and_spares_a_server_outside_the_pane() {
  let sandbox = Sandbox::new();
  let releases = sandbox.dir("releases");
  let mut pane = Pane::start(&sandbox, &releases, "stubborn");
  let server = pane.server_under();
  let outside = sandbox.dir("outside");
  harness::copy_file(&fake_pane_server(), &outside.join("charliebot-server"));
  let env = sandbox.env();
  let mut bystander_command = harness::command(outside.join("charliebot-server"), &env);
  bystander_command.arg("honest").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
  let mut bystander = harness::spawn(&mut bystander_command);

  let run = escalate(&sandbox, &releases, pane.pid());
  let bystander_alive = bystander.try_wait().expect("poll the bystander").is_none();
  stop(&mut bystander);

  assert!(bystander_alive, "a server outside the pane was signaled");
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(logged_with(&run.stdout, "SIGTERM", server), "{}", run.describe());
  assert!(logged_with(&run.stdout, "SIGKILL", server), "{}", run.describe());
  assert_eq!(pane.wait_exit(Duration::from_secs(5)), 0);
}

/// The escalation step against the pane shell, with a one-second SIGTERM wait.
fn escalate(sandbox: &Sandbox, releases: &Path, shell_pid: u32) -> Run {
  // impl_of_pid reads RELEASES_REAL, which run_detached sets; a sourced copy of the script needs it set too.
  let mut env = sandbox.env();
  env.insert("PANE".to_string(), "%escalation".to_string());
  env.insert("PANE_PID".to_string(), shell_pid.to_string());
  let releases_real = releases.canonicalize().expect("the release root");
  env.insert("RELEASES_REAL".to_string(), releases_real.display().to_string());
  env.insert("CHARLIEBOT_SWITCH_STOP_WAIT_S".to_string(), "1".to_string());
  let snippet = format!("source \"$1\" rust {} && escalate_pane", step_target_sha());
  let mut command = harness::command("bash", &env);
  command.args(["-c", snippet.as_str(), "bash"]).arg(repo_switch());
  harness::run(&mut command)
}

/// Whether one escalation log line names the signal and the pid it was sent to.
fn logged_with(output: &str, signal_name: &str, pid: u32) -> bool {
  let pid_word = Regex::new(&format!(r"\b{pid}\b")).expect("the pid pattern");
  output.lines().any(|line| line.contains(signal_name) && pid_word.is_match(line))
}

/// A pane's shell, whose one child is a stand-in server in `mode`. Dropping it kills what is left.
pub(crate) struct Pane {
  shell: Child,
  env: Env,
}

impl Pane {
  pub(crate) fn start(sandbox: &Sandbox, releases: &Path, mode: &str) -> Self {
    let directory = releases.join("pane");
    fs::create_dir_all(&directory).expect("the pane's release directory");
    let server = directory.join("charliebot-server");
    harness::copy_file(&fake_pane_server(), &server);
    let env = sandbox.env();
    require_setpriv(&env);
    let mut command = harness::command("bash", &env);
    command
      .args(["-c", "setpriv --pdeathsig KILL \"$1\" \"$2\"; true", "bash"])
      .arg(&server)
      .arg(mode)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null());
    let shell = harness::spawn(&mut command);
    Self { shell, env }
  }

  pub(crate) fn pid(&self) -> u32 {
    self.shell.id()
  }

  /// The stand-in server forked under the pane shell, once exec has run and its argv names charliebot-server.
  pub(crate) fn server_under(&self) -> u32 {
    wait_until(
      || children_of(&self.env, self.pid()).iter().any(|child| cmdline_contains(*child, "charliebot-server")),
      Duration::from_secs(10),
    );
    children_of(&self.env, self.pid())[0]
  }

  fn wait_exit(&mut self, timeout: Duration) -> i32 {
    let status = self
      .shell
      .wait_timeout(timeout)
      .expect("wait for the pane shell")
      .unwrap_or_else(|| panic!("the pane shell ran past {timeout:?}"));
    status.code().expect("the pane shell's exit code")
  }
}

impl Drop for Pane {
  fn drop(&mut self) {
    for child in children_of(&self.env, self.pid()) {
      match kill(Pid::from_raw(child as i32), Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => eprintln!("cannot kill pid {child}: {error}"),
      }
    }
    stop(&mut self.shell);
  }
}

/// Fails, naming the command, when the pane shell cannot set its server's parent-death signal: without setpriv the
/// server would outlive the test, which is the leak these checks close.
fn require_setpriv(env: &Env) {
  let on_path = env
    .get("PATH")
    .expect("the test environment sets PATH")
    .split(':')
    .map(|dir| Path::new(dir).join("setpriv"))
    .any(|candidate| candidate.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0));
  assert!(on_path, "the missing command is setpriv: the pane's shell starts its server through it");
  let mut command = harness::command("setpriv", env);
  command.arg("--help");
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "setpriv --help failed: {}", run.describe());
  assert!(run.stdout.contains("--pdeathsig"), "setpriv has no --pdeathsig option: {}", run.describe());
}

/// The pids of the processes whose parent is `pid`, from pgrep. pgrep exits 1 when none matches, which is no child.
fn children_of(env: &Env, pid: u32) -> Vec<u32> {
  let mut command = harness::command("pgrep", env);
  command.args(["-P", pid.to_string().as_str()]);
  let run = harness::run(&mut command);
  run.stdout.split_whitespace().map(|field| field.parse().expect("a pid")).collect()
}

fn cmdline_contains(pid: u32, needle: &str) -> bool {
  match fs::read(format!("/proc/{pid}/cmdline")) {
    Ok(bytes) => String::from_utf8_lossy(&bytes).contains(needle),
    // The process ended between the child listing and this read, so it runs no server.
    Err(error) if error.kind() == ErrorKind::NotFound => false,
    Err(error) => panic!("read the argv of pid {pid}: {error}"),
  }
}

// ---- the helper binary: the target release's `charliebot`, else the current release's ------------------------------

/// A release's `charliebot` that records each switch-helper call at `marker` and then runs the built stand-in.
fn logging_charliebot(marker: &Path) -> String {
  format!("printf '%s\\n' \"$*\" >> {}\nexec {} \"$@\"\n", quote(marker), quote(&fake_charliebot()))
}

#[test]
fn test_switch_helper_binary_falls_back_to_the_current_release_when_the_target_lacks_it() {
  let world = World::new();
  replace_charliebot(&world.releases.join(&world.new_sha), "exit 2\n");
  let marker = world.root().join("helper-calls.log");
  replace_charliebot(&world.releases.join(&world.old_sha), &logging_charliebot(&marker));

  let run = world.stubbed_preflight(&format!("rust {}", world.rust_sha), "rust", &world.new_sha);

  assert_eq!(run.code, 0, "{}", world.report(&run));
  let calls = fs::read_to_string(&marker).expect("the helper calls");
  assert!(calls.contains("switch-helper home-helper config"), "{calls}");
  assert!(calls.contains("switch-helper home-helper scan"), "{calls}");
}

#[test]
fn test_switch_helper_binary_uses_the_target_release_when_it_has_the_subcommand() {
  let world = World::new();
  replace_charliebot(&world.releases.join(&world.old_sha), "exit 2\n");
  let marker = world.root().join("helper-calls.log");
  replace_charliebot(&world.releases.join(&world.rust_sha), &logging_charliebot(&marker));

  let run = world.stubbed_preflight(&format!("rust {}", world.rust_sha), "rust", &world.rust_sha);

  assert_eq!(run.code, 0, "{}", world.report(&run));
  let calls = fs::read_to_string(&marker).expect("the helper calls");
  assert!(calls.contains("switch-helper home-helper config"), "{calls}");
}

#[test]
fn test_preflight_exits_2_when_neither_release_has_switch_helper() {
  let world = World::new();
  replace_charliebot(&world.releases.join(&world.rust_sha), "exit 2\n");
  replace_charliebot(&world.releases.join(&world.old_sha), "exit 2\n");
  let home_before = world.home_snapshot();
  let current_before = readlink_opt(&world.releases.join("current"));
  let rust_sha = world.rust_sha.clone();

  let run = world.switch(&["rust", &rust_sha]);

  assert_eq!(run.code, 2, "{}", world.report(&run));
  assert_eq!(readlink_opt(&world.releases.join("current")), current_before, "{}", world.report(&run));
  assert_eq!(world.git(&world.clone, &["rev-parse", "HEAD"]), world.old_sha, "{}", world.report(&run));
  assert_eq!(world.home_snapshot(), home_before, "{}", world.report(&run));
  assert!(world.report(&run).contains("neither release"), "{}", world.report(&run));
}

// ---- the switch on a real pane --------------------------------------------------------------------------------------

#[test]
fn test_switch_on_a_real_pane_restores_then_round_trips_then_exits_3() {
  let mut world = World::new();
  // This release ignores SIGTERM, so a zero stop wait reaches the SIGKILL path at once.
  world.env.insert("CHARLIEBOT_SWITCH_STOP_WAIT_S".to_string(), "0".to_string());
  let pane = world.start_pane();
  world.start_server(&pane);
  world.seed_live_run("run-live", Some("claude-opus"));
  let new_sha = world.new_sha.clone();
  let old_sha = world.old_sha.clone();

  // The new release's readback cannot adopt the live run: the switch fails after it stopped the old release and
  // restores it.
  write_behavior(&world.releases.join(&new_sha), &[("adopt", json!(false))]);
  let run = world.switch(&["rust", &new_sha]);
  assert_eq!(run.code, 2, "{}", world.report(&run));
  world.assert_server_scope_records(3);
  world.assert_untouched();

  // With a passing readback the live run is adopted, and the clone moves to the new release's commit. This release
  // ignores SIGTERM, so the switch back has to SIGKILL it.
  write_behavior(&world.releases.join(&new_sha), &[("ignore_sigterm", json!(true))]);
  let run = world.switch(&["rust", &new_sha]);
  assert_eq!(run.code, 0, "{}", world.report(&run));
  world.assert_server_scope_records(4);
  assert_eq!(fs::read_link(world.releases.join("current")).expect("current"), PathBuf::from(&new_sha));
  assert_eq!(world.git(&world.clone, &["rev-parse", "HEAD"]), new_sha);
  assert!(world.serves_release(&new_sha), "the new release does not serve");
  assert_eq!(world.version_status(), 200);
  assert!(
    world.latest_log().lines().filter_map(logline::parse).any(|row| row.field("run_id") == Some("run-live")),
    "{}",
    world.latest_log()
  );

  // The way back moves the clone to the earlier release commit.
  let run = world.switch(&["rust", &old_sha]);
  assert_eq!(run.code, 0, "{}", world.report(&run));
  world.assert_server_scope_records(5);
  assert_eq!(fs::read_link(world.releases.join("current")).expect("current"), PathBuf::from(&old_sha));
  assert_eq!(world.git(&world.clone, &["rev-parse", "HEAD"]), old_sha);
  assert!(world.serves_release(&old_sha), "the old release does not serve");

  // A switch whose new release cannot adopt, followed by a restore whose old release rejects its config, leaves the
  // server down, which is exit 3.
  write_behavior(&world.releases.join(&old_sha), &[("check_config", json!("fail"))]);
  write_behavior(&world.releases.join(&new_sha), &[("adopt", json!(false))]);
  let run = world.switch(&["rust", &new_sha]);
  assert_eq!(run.code, 3, "{}", world.report(&run));
  // The switch started the new release (one more scope) before its readback failed; the restore then could not start
  // the old release.
  world.assert_server_scope_records(6);
  assert!(world.listener_pids().is_empty(), "a server still listens on port {}", world.port);
}
