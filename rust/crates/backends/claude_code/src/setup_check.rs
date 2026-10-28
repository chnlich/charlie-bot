//! The Claude Code backend's setup check (`src/backends/claude_code/setup_check.py`): `setup_step` asserts the
//! command disallows the headless-unsafe tools. Registered as a setup step; `charliebot setup` runs it.

use std::collections::BTreeSet;

/// The tools the check requires in the command's `--disallowed-tools` value.
const REQUIRED: [&str; 5] = ["Monitor", "ScheduleWakeup", "CronCreate", "CronDelete", "CronList"];

/// Smoke-check the Claude Code backend command for headless-unsafe tools. Prints the banner and the
/// `OK:` line; the error names the missing tools the way Python's `SystemExit` did.
pub fn setup_step() -> Result<(), String> {
  println!("==> Checking Claude Code backend tools");
  let disallowed = super::base_disallowed_tools()?;
  let disallowed_tools: BTreeSet<&str> = disallowed.split(',').collect();
  let missing: Vec<&str> = REQUIRED.iter().copied().filter(|tool| !disallowed_tools.contains(tool)).collect();
  if !missing.is_empty() {
    return Err(format!("missing disallowed tools: {}", missing.join(",")));
  }
  println!("OK: backend disallows {}", REQUIRED.join(","));
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_check_passes_and_names_every_required_tool_in_the_ok_line() {
    // Python: tests/test_setup_runner.py::test_the_packages_register_the_cron_and_claude_code_steps_in_that_order
    // pins the step's presence; the output lines are the setup command's contract.
    setup_step().expect("the committed command disallows the required tools");
  }
}
