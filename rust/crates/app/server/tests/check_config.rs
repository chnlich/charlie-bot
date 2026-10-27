use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix::fcntl::{Flock, FlockArg};

fn server_command(home: &Path, args: &[&str], include_repo_root: bool) -> Command {
  let synthetic_root = home.parent().expect("the synthetic home has a parent");
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
  let mut command = Command::new(env!("CARGO_BIN_EXE_charliebot-server"));
  command
    .args(args)
    .current_dir(&repo_root)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("HOME", synthetic_root)
    .env("CHARLIEBOT_HOME", home)
    .env("CHARLIEBOT_VENV_BIN", synthetic_root.join("venv/bin"));
  if include_repo_root {
    command.env("CHARLIEBOT_REPO_ROOT", repo_root);
  }
  command
}

fn run_server(home: &Path, args: &[&str], include_repo_root: bool) -> Output {
  server_command(home, args, include_repo_root).output().expect("the server process starts")
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
  fn visit(root: &Path, directory: &Path, snapshot: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
      let entry = entry?;
      let path = entry.path();
      let relative = path.strip_prefix(root).expect("the entry is under the snapshot root").to_path_buf();
      if entry.file_type()?.is_dir() {
        snapshot.insert(relative, None);
        visit(root, &path, snapshot)?;
      } else {
        snapshot.insert(relative, Some(fs::read(path)?));
      }
    }
    Ok(())
  }

  let mut snapshot = BTreeMap::new();
  visit(root, root, &mut snapshot).expect("the synthetic home can be read");
  snapshot
}

#[test]
fn check_config_is_read_only_and_does_not_take_the_home_writer_fence() {
  let temporary = tempfile::tempdir().unwrap();
  let home = temporary.path().join("home");
  let venv_bin = temporary.path().join("venv/bin");
  fs::create_dir_all(home.join("state")).unwrap();
  fs::create_dir_all(&venv_bin).unwrap();
  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n",
  )
  .unwrap();
  fs::write(home.join("credentials.yaml"), "").unwrap();

  let lock_file = OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .truncate(false)
    .open(home.join("state/home_writer.lock"))
    .unwrap();
  let home_fence = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock).unwrap();
  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--check-config"], true);
  assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
  assert_eq!(output.stdout, b"config ok\n");
  assert_eq!(snapshot_tree(&home), before);

  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\nunknown_check_key: true\n",
  )
  .unwrap();
  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--check-config"], true);
  assert_eq!(output.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&output.stderr).contains("unknown_check_key"));
  assert_eq!(snapshot_tree(&home), before);

  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n",
  )
  .unwrap();
  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--check-config"], false);
  assert_eq!(output.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&output.stderr).contains("CHARLIEBOT_REPO_ROOT"));
  assert_eq!(snapshot_tree(&home), before);

  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--unknown"], true);
  assert_eq!(output.status.code(), Some(2));
  assert!(String::from_utf8_lossy(&output.stderr).contains("Usage: charliebot-server"));
  assert_eq!(snapshot_tree(&home), before);
  drop(home_fence);
}
