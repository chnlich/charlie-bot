//! The required-file reads: a `--file` argument or stdin; file reads carry Python's text-mode line endings.

use std::io::Read as _;
use std::path::Path;

use crate::exit::CliExit;

/// The file's text with Python's text-mode line endings, where `\r\n` and a lone `\r` read as
/// `\n` — what `Path.read_text(encoding="utf-8")` returns.
pub fn read_text_mode(path: &Path) -> Result<String, CliExit> {
  let content = std::fs::read_to_string(path).map_err(|error| CliExit::error(error.to_string()))?;
  Ok(content.replace("\r\n", "\n").replace('\r', "\n"))
}

/// A required text file, a usage error on a missing or empty one. The text carries Python's
/// text-mode line endings (see [`read_text_mode`]).
pub fn read_required_text_file(flag_name: &str, file_path: &str) -> Result<String, CliExit> {
  let path = Path::new(file_path);
  if !path.is_file() {
    return Err(CliExit::usage(format!("{flag_name} not found: {file_path}")));
  }
  let content = read_text_mode(path)?;
  if content.trim().is_empty() {
    return Err(CliExit::usage(format!("{flag_name} is empty: {file_path}")));
  }
  Ok(content)
}

/// The reply text: stdin when the arg is `-`, else the file's text.
pub fn read_reply_text(file_arg: &str) -> Result<String, CliExit> {
  if file_arg == "-" {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).map_err(|error| CliExit::error(error.to_string()))?;
    if text.trim().is_empty() {
      return Err(CliExit::usage("--file - read no text from stdin"));
    }
    return Ok(text);
  }
  read_required_text_file("--file", file_arg)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_text_file_reads_with_text_mode_line_endings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("message.txt");
    std::fs::write(&path, "first\r\nsecond\rthird\n").unwrap();

    let text = read_required_text_file("--file", path.to_str().unwrap()).unwrap();

    assert_eq!(text, "first\nsecond\nthird\n");
  }
}
