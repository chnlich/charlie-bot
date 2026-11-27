use std::fs;
use std::sync::mpsc;
use std::time::Duration;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::add::{session_slug8, write_capture};
use super::store;

fn run_with_timeout(test: impl FnOnce() + Send + 'static) {
  let (sender, receiver) = mpsc::channel();
  std::thread::spawn(move || {
    test();
    let _ = sender.send(());
  });
  receiver.recv_timeout(Duration::from_secs(2)).expect("the test finishes within two seconds");
}

fn fixed_clock() -> OffsetDateTime {
  OffsetDateTime::parse("2026-10-27T12:34:56Z", &Rfc3339).expect("the test timestamp parses")
}

#[test]
fn test_session_slug8_resolves_a_symlinked_sessions_root() {
  run_with_timeout(|| {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real-home").join("sessions").join("12345678-session-id");
    fs::create_dir_all(&real).unwrap();
    let linked = temp.path().join("linked-home");
    std::os::unix::fs::symlink(temp.path().join("real-home"), &linked).unwrap();
    assert_eq!(session_slug8(&linked.join("sessions"), &real), "12345678");
  });
}

#[test]
#[ignore = "integration"]
fn test_cli_add_creates_one_staging_file() {
  run_with_timeout(|| {
    let temp = tempfile::tempdir().unwrap();
    let home_dir = temp.path().join("home");
    let memory_dir = home_dir.join("memory");
    let sessions_root = home_dir.join("sessions");
    let cwd = sessions_root.join("12345678-session-id");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&memory_dir).unwrap();
    fs::write(memory_dir.join(crate::memory::TOPICS_FILENAME), "profile resident\n").unwrap();
    let body = "# Prefers Dark Mode\n\nThe user prefers dark themes across all UIs.\n";
    let target =
      write_capture(&memory_dir, &sessions_root, &cwd, "Prefers Dark Mode", body, || Ok(fixed_clock())).unwrap();
    let files = store::sorted_markdown_files(&memory_dir.join(crate::memory::STAGING_DIRNAME)).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(target.file_name().unwrap(), "20261027T123456Z-12345678-prefers-dark-mode.md");
    assert_eq!(fs::read_to_string(&target).unwrap(), body);
    assert!(store::entry_files(&memory_dir).unwrap().is_empty());
  });
}

#[test]
#[ignore = "integration"]
fn test_cli_add_on_a_fresh_home_creates_the_scaffold_and_the_capture() {
  run_with_timeout(|| {
    let temp = tempfile::tempdir().unwrap();
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&home_dir).unwrap();
    let memory_dir = home_dir.join("memory");
    let sessions_root = home_dir.join("sessions");
    let cwd = temp.path().to_path_buf();
    let body = "# Fresh Capture\n\nA fact to record.\n";
    let target = write_capture(&memory_dir, &sessions_root, &cwd, "Fresh Capture", body, || Ok(fixed_clock())).unwrap();
    for name in [".git", "topics", ".gitignore", "entries", "staging"] {
      assert!(memory_dir.join(name).exists(), "{name}");
    }
    assert_eq!(
      fs::read_to_string(memory_dir.join(crate::memory::TOPICS_FILENAME)).unwrap(),
      crate::memory::DEFAULT_MEMORY_TOPICS
    );
    assert_eq!(fs::read_to_string(memory_dir.join(".gitignore")).unwrap(), crate::memory::default_memory_gitignore());
    let files = store::sorted_markdown_files(&memory_dir.join(crate::memory::STAGING_DIRNAME)).unwrap();
    assert_eq!(files.as_slice(), std::slice::from_ref(&target));
    assert_eq!(fs::read_to_string(target).unwrap(), body);
  });
}
