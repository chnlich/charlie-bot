//! The startup lab's server launcher. `scripts/release/startup-lab.sh` hands it to trial.sh as the server binary; it
//! prepares the page cache and then execs the server named by `STARTUP_LAB_SERVER` with the same arguments and
//! environment, so the timed process is the real server.
//!
//! Every start first calls sync(): trial.sh copies the seed home just before the start, and the copy's dirty pages
//! would otherwise be written back while the server reads. A production start finds its files clean. Then, by
//! `STARTUP_LAB_CACHE`:
//! - `hot` reads every file under `CHARLIEBOT_HOME` twice. The second read moves the pages to the active list, so other
//!   jobs on a shared host do not reclaim them before the server reads them.
//! - `cold` drops the clean pages of every file under `CHARLIEBOT_HOME` and of the server binary
//!   (`POSIX_FADV_DONTNEED`), so the server reads them from disk. Directory entries and inodes stay cached, because
//!   dropping them needs root and affects the whole host.
//!
//! The exec keeps this process, so the server's read counters include this pass. The launcher logs its own count as
//! `wrapper_read_bytes` for `startup-lab.sh summary` to subtract.

use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use charliebot_core::home::CHARLIEBOT_HOME_ENV;
use walkdir::WalkDir;

#[derive(Clone, Copy, PartialEq)]
enum Cache {
  Hot,
  Cold,
}

fn required(name: &str, purpose: &str) -> Result<OsString> {
  env::var_os(name).filter(|value| !value.is_empty()).with_context(|| format!("{name} is unset; {purpose}"))
}

fn warm(path: &Path, buffer: &mut [u8]) -> Result<()> {
  for _ in 0..2 {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    while file.read(buffer).with_context(|| format!("cannot read {}", path.display()))? > 0 {}
  }
  Ok(())
}

fn evict(path: &Path) -> Result<()> {
  let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
  // SAFETY: the descriptor is open for the call's duration, and the advice changes only the page cache.
  let error = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
  if error != 0 {
    bail!("posix_fadvise {} failed: {}", path.display(), std::io::Error::from_raw_os_error(error));
  }
  Ok(())
}

/// The bytes this process has read from storage, from `/proc/self/io`.
fn own_read_bytes() -> Result<u64> {
  let io = std::fs::read_to_string("/proc/self/io").context("cannot read /proc/self/io")?;
  let value =
    io.lines().find_map(|line| line.strip_prefix("read_bytes: ")).context("/proc/self/io has no read_bytes")?;
  Ok(value.trim().parse()?)
}

fn launch() -> Result<std::convert::Infallible> {
  let server = PathBuf::from(required("STARTUP_LAB_SERVER", "startup-lab.sh sets it to the server under test")?);
  let cache = match required("STARTUP_LAB_CACHE", "startup-lab.sh sets it to hot or cold")?.to_str() {
    Some("hot") => Cache::Hot,
    Some("cold") => Cache::Cold,
    other => bail!("STARTUP_LAB_CACHE is hot or cold: {other:?}"),
  };
  let home = required(CHARLIEBOT_HOME_ENV, "trial.sh sets it to the trial home")?;
  // SAFETY: sync takes no arguments and only schedules writeback.
  unsafe { libc::sync() };
  let (mut files, mut bytes) = (0u64, 0u64);
  let mut buffer = vec![0u8; 1 << 20];
  for entry in WalkDir::new(&home) {
    let entry = entry.with_context(|| format!("the page-cache pass under {} failed", home.to_string_lossy()))?;
    if !entry.file_type().is_file() {
      continue;
    }
    match cache {
      Cache::Hot => warm(entry.path(), &mut buffer)?,
      Cache::Cold => evict(entry.path())?,
    }
    files += 1;
    bytes += entry.metadata()?.len();
  }
  if cache == Cache::Cold {
    evict(&server)?;
  }
  let read_bytes = own_read_bytes()?;
  let pass = if cache == Cache::Hot { "warmed" } else { "evicted" };
  eprintln!(
    "startup-lab: synced; {pass} {files} files, {bytes} bytes; wrapper_read_bytes={read_bytes}; exec {}",
    server.display()
  );
  let error = Command::new(&server)
    .arg0(&server)
    .args(env::args_os().skip(1))
    .env_remove("STARTUP_LAB_SERVER")
    .env_remove("STARTUP_LAB_CACHE")
    .exec();
  Err(error).with_context(|| format!("cannot exec {}", server.display()))
}

fn main() -> ExitCode {
  match launch() {
    Ok(never) => match never {},
    Err(error) => {
      eprintln!("startup-lab: {error:#}");
      ExitCode::FAILURE
    }
  }
}
