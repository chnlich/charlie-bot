//! Test helper that plays the fake programs of the agent tests.
//!
//! Cargo builds this binary once. Each fake program is a symlink named for its role (for example `claude`) that points
//! at this binary, next to a `probe.json` data file. The role's steps, the data file and the step vocabulary live in
//! `charliebot_agents::fake_probe`, which the artifacts crate's `chrome_stub` binary runs too. A run whose first
//! argument is the capture supervisor's starts that supervisor instead of playing a role.
//!
//! Tests link fakes to this binary and never write an executable and then run it. A fork in another test thread can
//! hold the written file open for writing, and the exec then fails with ETXTBSY.

use std::env;

fn main() {
  let args: Vec<_> = env::args_os().skip(1).collect();
  if args.first().is_some_and(|arg| arg == charliebot_agents::transport::final_stdout::SUPERVISOR_ARG) {
    charliebot_agents::transport::final_stdout::supervise(&args[1..]).expect("capture supervisor succeeds");
    return;
  }
  charliebot_agents::fake_probe::run();
}
