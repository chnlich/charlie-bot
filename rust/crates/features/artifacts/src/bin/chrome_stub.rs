//! Test helper that plays the fake headless Chrome of the artifacts tests.
//!
//! Cargo builds this binary once. Each fake is a symlink named `chrome` that points at this binary, next to a
//! `probe.json` data file. The role's steps, the data file and the step vocabulary live in
//! `charliebot_agents::fake_probe`, which the agents crate's `fake_program` binary runs too.
//!
//! Tests link fakes to this binary and never write an executable and then run it. A fork in another test thread can
//! hold the written file open for writing, and the exec then fails with ETXTBSY.

fn main() {
  charliebot_agents::fake_probe::run();
}
