//! A harness that runs the repository's shell scripts for real, against stub commands, for the Rust integration tests.
//!
//! A test makes a `Sandbox`: a temporary root whose `bin/` holds its stubs and comes first on PATH. The test adds
//! stubs with `Sandbox::stub`, builds a command with `command` and runs it with `run`. `spawn` starts a background
//! process. The README section "Shell-script tests" shows the calls.

mod api;
mod internals;

pub use api::{
  Env, RUN_TIMEOUT, Run, Sandbox, ScopeRecord, built_binary, command, copy_file, memtotal_percent_bytes, repo_root,
  run, run_with_stdin, spawn, write_executable,
};
