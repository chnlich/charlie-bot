//! Run one improve round's command under a write-only Landlock boundary.
//!
//! Usage: `improve-sandbox <direction> -- <command> [args...]`
//!
//! The boundary limits writes only: every read stays open, so the round reads the checkout, the cargo registry, the
//! mock data and the instructions file as before. Writes are allowed under the round's own directories and the caches
//! every direction shares, and denied everywhere else. The live CharlieBot home, the runtime clone, the other
//! directions' clones and the host worker's trees therefore stay out of reach of a round that writes the wrong path or
//! runs `rm`:
//!
//!   writable for this direction only
//!     ~/improve/<direction>/charlie-bot/     the direction's clone
//!     ~/improve/<direction>/evidence/        the round's screenshots, logs and summaries
//!     ~/improve/<direction>/clc-sessions/    the CLC reviewer's sessions
//!     /tmp/improve-<direction>/              the round's trial trees and scratch
//!     /tmp/improve-<direction>-tmp/          TMPDIR
//!     ~/.cache/improve-target/<direction>    the cargo target directory
//!   writable for every direction
//!     ~/improve/mock-home/                   the generated mock data and its manifest
//!     ~/.cargo/registry, ~/.cargo/git, ~/.cargo/.package-cache
//!     ~/.claude-improve                      the worker's own Claude config directory
//!   no write rule, so read-only inside the boundary
//!     ~/improve/home/                        config.yaml and credentials.yaml
//!     ~/improve/launch/<launch id>/          the instructions file and the in-flight files list
//!     ~/improve/<direction>/memory-peaks.tsv
//!
//! The device paths below carry write rules because tools write to them: /dev/null and the other null and random
//! devices, /dev/tty for an interactive tool, /dev/ptmx and /dev/pts for a new terminal, and /dev/shm for a browser's
//! shared memory.
//!
//! The program creates each missing allow-list directory, because the rules bind to objects that exist when it starts.
//! It stops with an error when the kernel cannot enforce Landlock: a round without the boundary would write anywhere
//! on the host. ABI 3 is required for the truncate rule. On success it replaces itself with the command, so the
//! command's exit code is the program's exit code.

use std::collections::BTreeSet;
use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The Landlock system call numbers. The kernel assigns the same numbers on every Linux architecture.
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
/// The ABI that adds the truncate right, which the write rules depend on.
const REQUIRED_ABI: i64 = 3;

/// Filesystem access rights from `linux/landlock.h`: bit 1 is write, bit 14 is truncate.
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
/// Every write right: bit 1 and bits 4 to 14 (write, remove and create of each object type, refer, truncate).
/// Bit 0 (execute) and bits 2 and 3 (read) stay out, so reads are never restricted.
const WRITE_ACCESS: u64 = 0x7FF2;
/// The rights that a rule on a regular file can carry; a directory-only right has no meaning there.
const FILE_ACCESS: u64 = ACCESS_FS_WRITE_FILE | ACCESS_FS_TRUNCATE;

const USAGE: &str = "usage: improve-sandbox <direction> -- <command> [args...]";

/// The device paths that tools write to. A missing one stops the program: a rule on a path that does not exist
/// would turn every later write to it into a denial.
const DEVICE_PATHS: [&str; 9] = [
  "/dev/null",
  "/dev/zero",
  "/dev/full",
  "/dev/random",
  "/dev/urandom",
  "/dev/tty",
  "/dev/ptmx",
  "/dev/pts",
  "/dev/shm",
];

/// `struct landlock_ruleset_attr`: the rights the ruleset handles.
#[repr(C)]
struct RulesetAttr {
  handled_access_fs: u64,
}

/// `struct landlock_path_beneath_attr`. The kernel declares it packed, so the fd sits right after the rights.
#[repr(C, packed)]
struct PathBeneathAttr {
  allowed_access: u64,
  parent_fd: i32,
}

/// Turn a negative system call result into the error of the last OS call, with the operation's name.
fn check(ret: i64, what: &str) -> Result<i64> {
  if ret < 0 {
    return Err(io::Error::last_os_error()).with_context(|| what.to_string());
  }
  Ok(ret)
}

/// A direction name is a lowercase word with digits and underscores, so it names one directory per round.
fn is_direction(name: &str) -> bool {
  let mut chars = name.chars();
  matches!(chars.next(), Some('a'..='z')) && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The writable paths of one direction, and the writable paths that every direction shares.
fn direction_paths(home: &Path, direction: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
  let round = home.join("improve").join(direction);
  let round_paths = vec![
    round.join("charlie-bot"),
    round.join("evidence"),
    round.join("clc-sessions"),
    PathBuf::from(format!("/tmp/improve-{direction}")),
    PathBuf::from(format!("/tmp/improve-{direction}-tmp")),
    home.join(".cache/improve-target").join(direction),
  ];
  let shared_paths = vec![
    home.join("improve/mock-home"),
    home.join(".cargo/registry"),
    home.join(".cargo/git"),
    home.join(".cargo/.package-cache"),
    home.join(".claude-improve"),
  ];
  (round_paths, shared_paths)
}

/// The direction and the command after `--`. The command must name a program.
fn parse_argv(args: &[OsString]) -> Result<(&str, &[OsString])> {
  let [direction, separator, command @ ..] = args else {
    bail!(USAGE);
  };
  if separator.as_bytes() != b"--" || command.is_empty() {
    bail!(USAGE);
  }
  let direction = direction.to_str().context(USAGE)?;
  if !is_direction(direction) {
    bail!("not an improve direction: {direction:?}");
  }
  Ok((direction, command))
}

/// Create each missing allow-list directory, after checking that every device path exists.
fn create_writable_directories(paths: &[PathBuf]) -> Result<()> {
  for device in DEVICE_PATHS {
    if !Path::new(device).exists() {
      bail!("device path {device} does not exist");
    }
  }
  for path in paths {
    // `lexists`: a dangling symlink counts as present, so the directory is not created over it.
    if !path.exists() && !path.is_symlink() {
      fs::create_dir_all(path).with_context(|| format!("cannot create {}", path.display()))?;
    }
  }
  Ok(())
}

/// The Landlock ABI version of this kernel. A kernel without Landlock stops the program.
fn landlock_abi() -> Result<i64> {
  // SAFETY: a null attribute with size 0 and the version flag only asks the kernel for its ABI version.
  let ret = unsafe {
    libc::syscall(
      SYS_LANDLOCK_CREATE_RULESET,
      std::ptr::null::<libc::c_void>(),
      0usize,
      LANDLOCK_CREATE_RULESET_VERSION,
    )
  };
  check(ret as i64, "this kernel cannot enforce Landlock; the round would run unguarded")
}

fn open_path(path: &Path) -> Result<OwnedFd> {
  let c_path = CString::new(path.as_os_str().as_bytes())
    .with_context(|| format!("a path holds a NUL byte: {}", path.display()))?;
  // SAFETY: c_path is NUL-terminated and lives across the call.
  let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
  check(fd as i64, &format!("open {}", path.display()))?;
  // SAFETY: a non-negative open result is a new descriptor that nothing else owns.
  Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Restrict this process's writes to `write_allowed`. Reads stay open. Returns the ABI version.
fn restrict(write_allowed: &BTreeSet<PathBuf>) -> Result<i64> {
  let abi = landlock_abi()?;
  if abi < REQUIRED_ABI {
    bail!("Landlock ABI {abi} cannot enforce truncate; ABI {REQUIRED_ABI} is required");
  }
  let attr = RulesetAttr { handled_access_fs: WRITE_ACCESS };
  // SAFETY: attr is a live struct of the size passed, and the kernel only reads it.
  let ruleset_fd = unsafe {
    libc::syscall(SYS_LANDLOCK_CREATE_RULESET, &attr as *const RulesetAttr, mem::size_of::<RulesetAttr>(), 0u32)
  };
  check(ruleset_fd as i64, "landlock_create_ruleset")?;
  // SAFETY: a non-negative create result is a new descriptor that nothing else owns.
  let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset_fd as RawFd) };

  for path in write_allowed {
    let access = if path.is_dir() { WRITE_ACCESS } else { WRITE_ACCESS & FILE_ACCESS };
    let parent = open_path(path)?;
    let rule = PathBeneathAttr { allowed_access: access, parent_fd: parent.as_raw_fd() };
    // SAFETY: rule is a live packed struct of the size the kernel expects, and the kernel only reads it.
    let ret = unsafe {
      libc::syscall(
        SYS_LANDLOCK_ADD_RULE,
        ruleset.as_raw_fd(),
        LANDLOCK_RULE_PATH_BENEATH,
        &rule as *const PathBeneathAttr,
        0u32,
      )
    };
    check(ret as i64, &format!("landlock_add_rule {}", path.display()))?;
  }

  // SAFETY: PR_SET_NO_NEW_PRIVS with these arguments reads and writes no memory.
  let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
  check(ret as i64, "prctl NO_NEW_PRIVS")?;
  // SAFETY: the ruleset descriptor is open and owned by this process.
  let ret = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset.as_raw_fd(), 0u32) };
  check(ret as i64, "landlock_restrict_self")?;
  Ok(abi)
}

fn main() -> Result<()> {
  let args: Vec<OsString> = std::env::args_os().skip(1).collect();
  let (direction, command) = parse_argv(&args)?;
  let home = std::env::home_dir().context("the home directory is unknown")?;
  let (round_paths, shared_paths) = direction_paths(&home, direction);
  let writable: Vec<PathBuf> = round_paths.into_iter().chain(shared_paths).collect();
  create_writable_directories(&writable)?;

  let mut write_allowed = BTreeSet::new();
  for path in writable.iter().map(PathBuf::as_path).chain(DEVICE_PATHS.iter().map(Path::new)) {
    write_allowed.insert(fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))?);
  }
  let abi = restrict(&write_allowed)?;
  eprintln!("improve boundary: write-only Landlock ABI {abi}, {} writable paths, {direction}", write_allowed.len());

  let error = Command::new(&command[0]).args(&command[1..]).exec();
  Err(error).with_context(|| format!("cannot start {}", command[0].to_string_lossy()))
}
