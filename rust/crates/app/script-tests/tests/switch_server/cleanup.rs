//! Checks that a stand-in server, and a pane's server, end with the test process that started them.
//!
//! `tests/common/hold.rs` says how a check holds a test process and kills it: the `hold_*` tests here are the ones
//! the checks start in a second copy of this binary.

// The hold module serves two test binaries; each uses part of it, so the rest is not dead code there.
#[path = "../common/hold.rs"]
#[allow(dead_code)]
mod hold;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use charliebot_shell_harness::Sandbox;

use crate::Pane;
use crate::world::World;

/// How long a held test waits to be killed. The check kills it long before this.
const HOLD_WAIT: Duration = Duration::from_secs(60);

/// The version wait panics inside the `FakeServer` guard's life, so the stand-in stops on unwind.
#[test]
fn a_panicking_version_wait_stops_the_stand_in_server() {
  let world = World::new();
  let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    world.start_fake_server_when(&[], |_| false);
  }));

  assert!(panicked.is_err(), "a readiness that never holds must panic");
  assert!(
    hold::within(hold::END_WAIT, || hold::processes_under(world.root()).is_empty()),
    "a stand-in under {} outlived the panicking version wait: {:?}",
    world.root().display(),
    hold::processes_under(world.root())
  );
}

/// A stand-in server ends with its test process, which is killed here with SIGKILL.
#[test]
fn the_stand_in_server_ends_with_a_killed_test_process() {
  let mut held_test = hold::HeldTest::start("cleanup::hold_a_stand_in_server_for_the_kill_check");
  let held = Held::read(&held_test.recorded());
  assert!(held.pids.iter().all(|pid| !hold::has_ended(*pid)), "recorded pids do not run: {:?}", held.pids);
  held_test.kill_and_reap();

  assert!(
    hold::within(hold::END_WAIT, || hold::processes_under(&held.root).is_empty()),
    "processes under {} outlived the killed test: {:?}",
    held.root.display(),
    hold::processes_under(&held.root)
  );
  assert!(held.pids.iter().all(|pid| hold::has_ended(*pid)), "recorded pids still run: {:?}", held.pids);
  remove_killed_test_root(&held.root);
}

/// The pane shell and the server under it end with the test process, which is killed here with SIGKILL.
#[test]
fn the_pane_server_ends_with_a_killed_test_process() {
  let mut held_test = hold::HeldTest::start("cleanup::hold_a_pane_for_the_kill_check");
  let held = Held::read(&held_test.recorded());
  assert!(held.pids.iter().all(|pid| !hold::has_ended(*pid)), "recorded pids do not run: {:?}", held.pids);
  held_test.kill_and_reap();

  assert!(
    hold::within(hold::END_WAIT, || hold::processes_under(&held.root).is_empty()),
    "processes under {} outlived the killed test: {:?}",
    held.root.display(),
    hold::processes_under(&held.root)
  );
  assert!(held.pids.iter().all(|pid| hold::has_ended(*pid)), "recorded pids still run: {:?}", held.pids);
  remove_killed_test_root(&held.root);
}

/// Started by `the_stand_in_server_ends_with_a_killed_test_process` in a second copy of this binary. Without
/// `LEAK_CHECK_HOLD` it does nothing, so an ordinary run of this file passes it.
#[test]
fn hold_a_stand_in_server_for_the_kill_check() {
  let Some(marker) = hold::marker() else { return };
  let world = World::new();
  let server = world.start_fake_server(&[]);
  Held { root: world.root().to_path_buf(), pids: vec![server.pid()] }.write(&marker);
  thread::sleep(HOLD_WAIT);
}

/// Started by `the_pane_server_ends_with_a_killed_test_process` in a second copy of this binary. Without
/// `LEAK_CHECK_HOLD` it does nothing, so an ordinary run of this file passes it.
#[test]
fn hold_a_pane_for_the_kill_check() {
  let Some(marker) = hold::marker() else { return };
  let sandbox = Sandbox::new();
  let releases = sandbox.dir("releases");
  let pane = Pane::start(&sandbox, &releases, "honest");
  let server = pane.server_under();
  Held { root: sandbox.root().to_path_buf(), pids: vec![pane.pid(), server] }.write(&marker);
  thread::sleep(HOLD_WAIT);
}

/// What a held test recorded: the test root its processes run under, and their pids.
struct Held {
  root: PathBuf,
  pids: Vec<u32>,
}

/// A test process killed with SIGKILL never drops its sandbox, so its root stays on disk; the check removes it.
fn remove_killed_test_root(root: &Path) {
  if let Err(error) = fs::remove_dir_all(root) {
    eprintln!("cannot remove the killed test's root {}: {error}", root.display());
  }
}

impl Held {
  fn read(marker: &str) -> Self {
    let mut root = None;
    let mut pids = Vec::new();
    for line in marker.lines() {
      if let Some(value) = line.strip_prefix("root=") {
        root = Some(PathBuf::from(value));
      } else if let Some(value) = line.strip_prefix("pid=") {
        pids.push(value.parse().expect("a pid in the marker"));
      }
    }
    Self { root: root.expect("the marker names the test root"), pids }
  }

  fn write(&self, marker: &Path) {
    let mut text = format!("root={}\n", self.root.display());
    for pid in &self.pids {
      text.push_str(&format!("pid={pid}\n"));
    }
    hold::write_marker(marker, &text);
  }
}
