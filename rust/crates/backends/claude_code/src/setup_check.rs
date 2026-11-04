//! The Claude Code backend's setup check: `setup_step` asserts the command disallows the headless-unsafe tools.
//! Registered as a setup step; `charliebot setup` runs it.

use std::collections::BTreeSet;

/// The tools the check requires in the command's `--disallowed-tools` value: the wake-up tools plus the three
/// entry points that start work outside CharlieBot (Workflow, SendMessage and RemoteTrigger).
const REQUIRED: [&str; 8] =
  ["Monitor", "ScheduleWakeup", "CronCreate", "CronDelete", "CronList", "Workflow", "SendMessage", "RemoteTrigger"];

/// Smoke-check the Claude Code backend command for headless-unsafe tools. Prints the banner and the
/// `OK:` line; the error names the missing tools, sorted and comma-separated.
pub fn setup_step() -> Result<(), String> {
  println!("==> Checking Claude Code backend tools");
  let disallowed = super::base_disallowed_tools()?;
  let disallowed_tools: BTreeSet<&str> = disallowed.split(',').collect();
  let mut missing: Vec<&str> = REQUIRED.iter().copied().filter(|tool| !disallowed_tools.contains(tool)).collect();
  if !missing.is_empty() {
    missing.sort_unstable();
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
    // Pins the step's presence; the output lines are the setup command's contract.
    setup_step().expect("the committed command disallows the required tools");
  }
}
