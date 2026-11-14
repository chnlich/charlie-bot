//! A finished process's status as the integer its callers report (`code_of`).
//!
//! The server reports a signalled process as the negative signal number, the form `Popen.returncode` and the run
//! records use. Every writer and reader spells that rule here once.

use std::process::ExitStatus;

/// The integer `status` reports: its exit code, or the negative number of the signal that ended the process.
///
/// `None` only when the status carries neither, which a Unix status does not; each caller picks its own sentinel.
pub fn code_of(status: ExitStatus) -> Option<i32> {
  use std::os::unix::process::ExitStatusExt;

  status.code().or_else(|| status.signal().map(|signal| -signal))
}

#[cfg(test)]
mod tests {
  use std::os::unix::process::ExitStatusExt;
  use std::process::ExitStatus;

  use super::code_of;

  #[test]
  fn an_exit_code_reports_as_itself() {
    assert_eq!(code_of(ExitStatus::from_raw(7 << 8)), Some(7));
    assert_eq!(code_of(ExitStatus::from_raw(0)), Some(0));
  }

  #[test]
  fn a_signal_death_reports_as_the_negative_signal_number() {
    assert_eq!(code_of(ExitStatus::from_raw(15)), Some(-15));
    assert_eq!(code_of(ExitStatus::from_raw(9)), Some(-9));
  }
}
