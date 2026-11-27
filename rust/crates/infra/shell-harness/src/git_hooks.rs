use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use crate::repo_root;

const PRE_RECEIVE: &str = include_str!("pre_receive_leak_scan.sh");

/// Installs the real repository leak scanner on a synthetic bare origin.
///
/// The hook scans every received ref name and every newly received commit message. The scratch directory owns the
/// isolated scanner environment, including its local pattern file and empty credential directories.
pub fn install_pre_receive_leak_scan(origin: &Path, scratch: &Path) -> io::Result<()> {
  let fixture = scratch.join("land-queue-leak-scan");
  let home = fixture.join("home");
  let claude = fixture.join("claude");
  let codex = fixture.join("codex");
  for directory in [&home, &claude, &codex] {
    fs::create_dir_all(directory)?;
  }
  let patterns = fixture.join("patterns.txt");
  fs::write(&patterns, "# local leak patterns\nfictionalmarker\n")?;

  let hooks = origin.join("hooks");
  fs::create_dir_all(&hooks)?;
  let hook = hooks.join("pre-receive");
  fs::write(&hook, PRE_RECEIVE)?;
  fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;

  let scanner = repo_root().join("dev/check-skills-host-leak.sh");
  let config = [
    ("charliebot.queueTestScanner", scanner.as_path()),
    ("charliebot.queueTestPatterns", patterns.as_path()),
    ("charliebot.queueTestHome", home.as_path()),
    ("charliebot.queueTestClaude", claude.as_path()),
    ("charliebot.queueTestCodex", codex.as_path()),
  ];
  for (key, value) in config {
    let output = Command::new("git").arg("-C").arg(origin).args(["config", "--local", key]).arg(value).output()?;
    if !output.status.success() {
      return Err(io::Error::other(format!(
        "git config {key} failed in {}: {}",
        origin.display(),
        String::from_utf8_lossy(&output.stderr).trim()
      )));
    }
  }
  Ok(())
}
