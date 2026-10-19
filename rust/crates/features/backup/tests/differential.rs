//! The differential check: Python's `create_backup` and this crate's archive the same synthetic home,
//! and a Python driver compares the two archives through Python's `tarfile` — the member list in
//! order, each member's metadata and PAX records, and each regular member's content.
//!
//! Each side writes into its own backup directory: Python's beside the home, this crate's beside a
//! symlink to the same home. The control run plants one difference, and the driver must report it.
//!
//! Run with `cargo test --release -p charliebot-backup --test differential -- --nocapture`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Writes one fixture file, creating its parent directories.
fn write(home: &Path, relative: impl AsRef<Path>, content: &[u8]) {
  let path = home.join(relative);
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).unwrap();
  }
  std::fs::write(path, content).unwrap();
}

/// Builds the synthetic home the two sides archive: nested directories, an empty directory, a credentials
/// file, a `.git` directory, a `.claude` directory, a pycache file, an opencode store at the top level and one
/// deeper, a session thread directory, names that are not UTF-8 at the top level and nested, a non-ASCII
/// UTF-8 name, a name over 100 bytes, and symlinks whose targets are over 100 bytes or not UTF-8.
fn build_fixture_home(home: &Path) {
  write(home, "config.yaml", b"server: {}\n");
  write(home, "nested/a/b/deep.txt", b"deep content\n");
  write(home, OsStr::from_bytes(b"raw-\xff\xfe.txt"), b"a top-level name that is not UTF-8\n");
  write(home, OsStr::from_bytes(b"nested/a/bad-\xc3(.txt"), b"a nested name with a truncated sequence\n");
  write(home, OsStr::from_bytes(b"nested/raw-dir-\xff/inner-\x80.txt"), b"inside a directory that is not UTF-8\n");
  write(home, "nested/a/café-日本.txt", b"a non-ASCII UTF-8 name\n");
  // Python sorts names by code point: the raw `\xf0` byte is U+DCF0, below U+E000, so the raw name comes first.
  // Byte order would put the private-use name first.
  write(home, OsStr::from_bytes(b"\xf0-raw.txt"), b"sorts before the private-use name\n");
  write(home, "\u{e000}-private.txt", b"sorts after the raw name\n");
  write(home, format!("{}.txt", "n".repeat(110)), b"a name over 100 bytes\n");
  std::fs::create_dir_all(home.join("empty_dir")).unwrap();
  std::os::unix::fs::symlink("nested", home.join("link")).unwrap();
  std::os::unix::fs::symlink("target-".repeat(15), home.join("long-link")).unwrap();
  std::os::unix::fs::symlink(OsStr::from_bytes(b"raw-target-\xff"), home.join("raw-link")).unwrap();
  write(home, "credentials.yaml", b"credentials:\n  access_key: secret\n");
  write(home, ".git/HEAD", b"ref: refs/heads/main\n");
  write(home, ".claude/settings.json", b"{}\n");
  write(home, "__pycache__/x.pyc", b"bytecode\n");
  write(home, "cache/y.pyc", b"more bytecode\n");
  write(home, "opencode/data/opencode/opencode.db", b"db\n");
  write(home, "sessions/s1/opencode/note.json", b"{}\n");
  write(home, "sessions/s1/threads/t1/metadata.json", b"{}\n");
}

/// Builds the fixture under `root` and archives it on the Rust side. The home is `root/charliebot`, and the
/// Rust side reads it through the symlink `root/rust/charliebot`, so both sides see the same metadata and
/// write into different backup directories: `root/charliebot_backup` for Python, `root/rust/charliebot_backup`
/// for this crate.
fn archive_fixture(root: &Path) -> (PathBuf, PathBuf) {
  let home = root.join("charliebot");
  std::fs::create_dir(&home).unwrap();
  build_fixture_home(&home);
  std::fs::create_dir(root.join("rust")).unwrap();
  let rust_home = root.join("rust").join("charliebot");
  std::os::unix::fs::symlink(&home, &rust_home).unwrap();
  let rust_archive = charliebot_backup::backup::create_backup_in(&rust_home).unwrap();
  (home, rust_archive)
}

/// Runs the Python comparison driver on one home and one Rust archive, with the driver's extra arguments.
fn run_driver(home: &Path, rust_archive: &Path, extra: &[&str]) -> Output {
  let repo = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap();
  let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/differential_compare.py");
  let mut command = Command::new("uv");
  command.args(["run", "python"]).arg(&script).arg(home).arg(rust_archive).args(extra).current_dir(repo);
  println!("command: {command:?}");
  command.output().expect("running the Python comparison driver")
}

/// Prints the driver's stdout (its summary and differences) and stderr.
fn report(output: &Output) {
  println!("{}", String::from_utf8_lossy(&output.stdout));
  eprintln!("{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn differential_backup_matches_python() {
  let root = tempfile::tempdir().unwrap();
  let (home, rust_archive) = archive_fixture(root.path());

  let output = run_driver(&home, &rust_archive, &[]);
  report(&output);
  assert!(output.status.success(), "the differential comparison found differences or failed to run");
  assert!(String::from_utf8_lossy(&output.stdout).contains("differences: 0"));
}

#[test]
fn differential_control_run_reports_one_planted_difference() {
  let root = tempfile::tempdir().unwrap();
  let (home, rust_archive) = archive_fixture(root.path());

  let output = run_driver(&home, &rust_archive, &["--plant-difference"]);
  report(&output);
  assert!(!output.status.success(), "the control run must fail on its planted difference");
  assert!(String::from_utf8_lossy(&output.stdout).contains("differences: 1"));
}
