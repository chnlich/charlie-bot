//! The differential check: Python's `create_backup` and this crate's archive the same synthetic home,
//! and a Python driver compares the two archives through Python's `tarfile` — the member list in
//! order, each member's metadata, and each regular member's content.
//!
//! Run with `cargo test --release -p charliebot-backup --test differential`.

use std::path::Path;
use std::process::Command;

/// Builds the synthetic home the two sides archive: nested directories, a symlink, an empty directory,
/// a credentials file, a `.git` directory, a `.claude` directory, a pycache file, an opencode store at
/// the top level and one deeper, and a session thread directory.
fn build_fixture_home(home: &Path) {
  let write = |relative: &str, content: &[u8]| {
    let path = home.join(relative);
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
  };
  write("config.yaml", b"server: {}\n");
  write("nested/a/b/deep.txt", b"deep content\n");
  std::fs::create_dir_all(home.join("empty_dir")).unwrap();
  std::os::unix::fs::symlink("nested", home.join("link")).unwrap();
  write("credentials.yaml", b"credentials:\n  access_key: secret\n");
  write(".git/HEAD", b"ref: refs/heads/main\n");
  write(".claude/settings.json", b"{}\n");
  write("__pycache__/x.pyc", b"bytecode\n");
  write("cache/y.pyc", b"more bytecode\n");
  write("opencode/data/opencode/opencode.db", b"db\n");
  write("sessions/s1/opencode/note.json", b"{}\n");
  write("sessions/s1/threads/t1/metadata.json", b"{}\n");
}

#[test]
fn differential_backup_matches_python() {
  let repo = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap();
  let home = tempfile::tempdir().unwrap();
  build_fixture_home(home.path());

  unsafe { std::env::set_var(charliebot_core::home::CHARLIEBOT_HOME_ENV, home.path()) };
  let rust_archive = charliebot_backup::backup::create_backup().unwrap();
  unsafe { std::env::remove_var(charliebot_core::home::CHARLIEBOT_HOME_ENV) };

  let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/differential_compare.py");
  let output = Command::new("uv")
    .args(["run", "python"])
    .arg(&script)
    .arg(home.path())
    .arg(&rust_archive)
    .current_dir(repo)
    .output()
    .expect("running the Python comparison driver");
  println!("{}", String::from_utf8_lossy(&output.stdout));
  eprintln!("{}", String::from_utf8_lossy(&output.stderr));
  assert!(output.status.success(), "the differential comparison found differences or failed to run");
}
