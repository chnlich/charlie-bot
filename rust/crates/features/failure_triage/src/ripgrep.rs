//! The crate's one interface to the `rg` binary.
//!
//! A scan asks ripgrep twice: `preflight` checks that the binary answers, and LESSONS retrieval ranks one file's
//! lines by token. Both calls live here, so the binary name, the install guidance and the `--json` line format
//! have one home.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// One line ripgrep reported as a match, with the line number it starts at, from 1.
pub(crate) struct MatchLine {
  pub(crate) line_number: usize,
  pub(crate) text: String,
}

/// Check that ripgrep answers, for a scan that is about to rank LESSONS entries.
pub(crate) fn ensure_available() -> Result<()> {
  let answered = Command::new("rg")
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|status| status.success());
  if !answered {
    bail!("rg is not available: install ripgrep so failure triage can rank LESSONS entries");
  }
  Ok(())
}

/// Every line of `file` that holds one of `patterns`, matched case-insensitively as fixed strings.
/// A search with no match is an empty list; any other failure is an error.
pub(crate) fn matching_lines(file: &Path, patterns: &[String]) -> Result<Vec<MatchLine>> {
  let mut command = Command::new("rg");
  command.arg("--json").arg("--no-config").arg("--color").arg("never").arg("-i").arg("-F");
  for pattern in patterns {
    command.arg("-e").arg(pattern);
  }
  command.arg("--").arg(file);
  let output = command.output().with_context(|| format!("running rg over {}", file.display()))?;
  if !output.status.success() && output.status.code() != Some(1) {
    bail!("rg failed to search {}: {}", file.display(), String::from_utf8_lossy(&output.stderr).trim());
  }
  Ok(parse_matches(&output.stdout))
}

/// The match lines in ripgrep's `--json` output. Other message kinds and malformed lines carry no match.
fn parse_matches(stdout: &[u8]) -> Vec<MatchLine> {
  let mut lines = Vec::new();
  for line in String::from_utf8_lossy(stdout).lines() {
    let Ok(message) = serde_json::from_str::<Value>(line) else { continue };
    if message.get("type").and_then(Value::as_str) != Some("match") {
      continue;
    }
    let Some(data) = message.get("data") else { continue };
    let Some(line_number) = data.get("line_number").and_then(Value::as_u64) else { continue };
    let Some(text) = data.get("lines").and_then(|lines| lines.get("text")).and_then(Value::as_str) else {
      continue;
    };
    lines.push(MatchLine { line_number: line_number as usize, text: text.to_string() });
  }
  lines
}
