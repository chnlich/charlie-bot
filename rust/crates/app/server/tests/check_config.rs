use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix::fcntl::{Flock, FlockArg};

fn prepare_server_bin_dir(root: &Path, with_cli: bool) -> PathBuf {
  let directory = root.join("server-bin");
  fs::create_dir_all(&directory).unwrap();
  let server = directory.join("charliebot-server");
  fs::copy(env!("CARGO_BIN_EXE_charliebot-server"), &server).unwrap();
  let mut permissions = fs::metadata(&server).unwrap().permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&server, permissions).unwrap();
  if with_cli {
    write_fake_cli(&directory);
  }
  directory
}

fn write_fake_cli(directory: &Path) {
  let cli = directory.join("charliebot");
  fs::write(&cli, "test fixture; this file is not run\n").unwrap();
  let mut permissions = fs::metadata(&cli).unwrap().permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&cli, permissions).unwrap();
}

fn server_command(home: &Path, args: &[&str], include_repo_root: bool, server_bin_dir: &Path) -> Command {
  let synthetic_root = home.parent().expect("the synthetic home has a parent");
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
  let mut command = Command::new(server_bin_dir.join("charliebot-server"));
  command
    .args(args)
    .current_dir(&repo_root)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("HOME", synthetic_root)
    .env("CHARLIEBOT_HOME", home);
  if include_repo_root {
    command.env("CHARLIEBOT_REPO_ROOT", repo_root);
  }
  command
}

fn run_server(home: &Path, args: &[&str], include_repo_root: bool, server_bin_dir: &Path) -> Output {
  server_command(home, args, include_repo_root, server_bin_dir).output().expect("the server process starts")
}

/// A valid approval policy table: the check-config home carries one, and the check writes nothing into the home.
fn approval_policy_table() -> String {
  "\
consequence_tests:
  - {id: irreversible, rank: 1, text: \"Deletes data with no backup, or sends what cannot be recalled.\"}
  - {id: chao-name, rank: 2, text: \"A message or publication in Chao's name reaches other people.\"}
  - {id: money, rank: 3, text: \"Spends money beyond a budget that Chao set.\"}
  - {id: authority, rank: 4, text: \"Widens what agents may do.\"}
  - {id: rule-named, rank: 4, text: \"A rule in force names the user's explicit approval for this step.\"}
  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}
standing_policies: []
enable_ask_tier_refusal: false
"
  .to_string()
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
  let server_bin_dir = prepare_server_bin_dir(temporary.path(), false);
  fs::create_dir_all(home.join("state")).unwrap();
  fs::create_dir_all(home.join("config.d")).unwrap();
  fs::write(home.join("config.d/approval_policy.yaml"), approval_policy_table()).unwrap();
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
  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(1));
  let expected_cli = server_bin_dir.join("charliebot");
  assert!(
    String::from_utf8_lossy(&output.stderr).contains(&expected_cli.display().to_string()),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  write_fake_cli(&server_bin_dir);
  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
  assert_eq!(output.stdout, b"config ok\n");
  assert_eq!(snapshot_tree(&home), before);

  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\nunknown_check_key: true\n",
  )
  .unwrap();
  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&output.stderr).contains("unknown_check_key"));
  assert_eq!(snapshot_tree(&home), before);

  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n",
  )
  .unwrap();
  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--check-config"], false, &server_bin_dir);
  assert_eq!(output.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&output.stderr).contains("CHARLIEBOT_REPO_ROOT"));
  assert_eq!(snapshot_tree(&home), before);

  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--unknown"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(2));
  assert!(String::from_utf8_lossy(&output.stderr).contains("Usage: charliebot-server"));
  assert_eq!(snapshot_tree(&home), before);
  drop(home_fence);
}
