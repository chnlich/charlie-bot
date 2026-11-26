//! The `charliebot switch-helper` subcommands: the reads `scripts/switch-server.sh` performs on the profile home, on
//! a run record and on an API response body.
//!
//! The switch script runs the `charliebot` binary of the release it reads with (see the script's header). Every
//! helper prints exactly what the switch compares and exits with the code it branches on. A helper that matched
//! prints nothing and exits 0. One that did not match exits 1. The nine names each take the arguments the switch
//! passes. `reattach REPO SHA BACKEND_TYPE` is an operator's check of one backend type's re-attachment declaration at
//! a build. The switch does not call it.

mod api;
mod common;
mod entry;
mod home;
mod process;
mod scan;

pub use entry::{SUBCOMMAND, main};
