//! The foreground command cap: every shell command an agent runs ends at the deadline.
//!
//! A `PreToolUse` hook rewrites the command into `charliebot foreground-call`, which runs it in a call cgroup under the
//! session cgroup. A guard transient unit, outside the call cgroup, ends the call at the deadline. The call entry, the
//! guard and the hook answers live in this module; each backend reaches them through the constants and the hook
//! subcommand.

pub mod agy_hooks;
pub mod call;
pub mod devin_config;
pub mod guard;
pub mod hook;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use charliebot_core::cgroup::{agents_slice_dir, session_cgroup_path};

pub use charliebot_core::cgroup::SESSION_CGROUP_ENV;

/// The deadline of one foreground call, in seconds. The guard sends SIGTERM at this point.
pub const FOREGROUND_DEADLINE_SECS: u64 = 895;
/// The grace between SIGTERM and `cgroup.kill`, in seconds.
pub const FOREGROUND_STOP_SECS: u64 = 5;

// The guard's SIGKILL lands within the 15-minute limit that a foreground command keeps.
const _: () = assert!(FOREGROUND_DEADLINE_SECS + FOREGROUND_STOP_SECS <= 900);

/// The `PreToolUse` hook command that rewrites a shell command into the call entry.
pub const HOOK_COMMAND: &str = "charliebot foreground-cap";

/// The name of the global agy hook entry that runs `HOOK_COMMAND`.
pub const AGY_HOOK_KEY: &str = "charliebot-foreground-cap";

/// The name prefix of each call's cgroup under the session cgroup. Not `charliebot-sess-`, so the session sweep never
/// reads a call directory as a session.
const CALL_DIR_PREFIX: &str = "call-";

/// The name prefix of each call's guard unit.
const GUARD_UNIT_PREFIX: &str = "charliebot-fg-";

/// The exit status of a command that the deadline ended: the SIGTERM status.
pub const DEADLINE_EXIT_CODE: i32 = 143;

/// The session cgroup of the session `session_id` under the delegated cgroup base `base_dir`. The agent runs place
/// their environment variable on this path, and the session cgroup holds it once the spawn creates it.
pub fn session_cgroup_dir(base_dir: &Path, session_id: &str) -> PathBuf {
  session_cgroup_path(&agents_slice_dir(base_dir), session_id)
}

/// The error text the agent's tool result carries when the deadline ended the command.
pub fn deadline_error_text(deadline_secs: u64) -> String {
  format!(
    "charliebot foreground deadline: command ended at the {deadline_secs} s limit (exit {DEADLINE_EXIT_CODE}). \
     Background jobs it started ended with it; a job that must outlive it moves into ${SESSION_CGROUP_ENV}."
  )
}

/// The shell command a hook writes for `command`: the call entry with the command hex-encoded, so the command passes
/// as one argument whatever characters it holds.
pub fn call_command(command: &str) -> String {
  format!("charliebot foreground-call --cmd-hex {}", encode_hex(command))
}

/// Hex-encode `text`, two lowercase digits for each byte.
pub fn encode_hex(text: &str) -> String {
  text.bytes().fold(String::with_capacity(text.len() * 2), |mut out, byte| {
    write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    out
  })
}

/// Decode the hex text that `encode_hex` produced.
pub fn decode_hex(hex: &str) -> Result<String, String> {
  if !hex.len().is_multiple_of(2) {
    return Err(format!("the command hex has an odd length ({})", hex.len()));
  }
  let bytes = (0..hex.len())
    .step_by(2)
    .map(|index| {
      u8::from_str_radix(&hex[index..index + 2], 16)
        .map_err(|error| format!("the command hex has an invalid digit at {index}: {error}"))
    })
    .collect::<Result<Vec<u8>, String>>()?;
  String::from_utf8(bytes).map_err(|error| format!("the command hex is not UTF-8: {error}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_deadline_and_the_stop_grace_are_the_planned_values() {
    // The 895 + 5 <= 900 bound is a compile-time assertion above, so this test pins the two values.
    assert_eq!(FOREGROUND_DEADLINE_SECS, 895);
    assert_eq!(FOREGROUND_STOP_SECS, 5);
  }

  #[test]
  fn the_error_text_names_the_deadline_the_exit_and_the_session_variable() {
    let text = deadline_error_text(895);
    assert_eq!(
      text,
      "charliebot foreground deadline: command ended at the 895 s limit (exit 143). Background jobs it started \
       ended with it; a job that must outlive it moves into $CHARLIEBOT_SESSION_CGROUP."
    );
    assert!(deadline_error_text(15).contains("at the 15 s limit"));
  }

  #[test]
  fn a_hex_command_round_trips_through_the_call_command() {
    let command = "echo 'BEFORE'; sleep 25 \"x\" && echo \u{e9}";
    let line = call_command(command);
    let hex = line.strip_prefix("charliebot foreground-call --cmd-hex ").expect("the call line names the entry");
    assert_eq!(decode_hex(hex).unwrap(), command);
  }

  #[test]
  fn an_odd_or_non_hex_command_is_refused() {
    assert!(decode_hex("abc").unwrap_err().contains("odd length"));
    assert!(decode_hex("zz").unwrap_err().contains("invalid digit"));
  }

  #[test]
  fn the_session_cgroup_dir_sits_under_the_agents_slice() {
    let dir = session_cgroup_dir(Path::new("/base"), "abcd1234-ef56-7890-abcd-ef1234567890");
    assert_eq!(dir.parent().unwrap(), agents_slice_dir(Path::new("/base")));
    assert!(dir.file_name().unwrap().to_string_lossy().starts_with("charliebot-sess-"));
  }
}
