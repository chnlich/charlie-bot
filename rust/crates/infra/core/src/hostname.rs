//! The local host's name: the usage ledger keys a capture with it, and a page shows it.

use std::ffi::OsString;

/// This host's name, read from the kernel. The caller renders it and decides how a failure reads.
pub fn os_name() -> Result<OsString, nix::errno::Errno> {
  nix::unistd::gethostname()
}
