//! The required-file reads (`src/runtime/cli/common.py`).

use std::io::Read as _;
use std::path::Path;

use crate::exit::CliExit;

/// `read_required_text_file`: a required text file, a usage error on a missing or empty one.
pub fn read_required_text_file(flag_name: &str, file_path: &str) -> Result<String, CliExit> {
  let path = Path::new(file_path);
  if !path.is_file() {
    return Err(CliExit::usage(format!("{flag_name} not found: {file_path}")));
  }
  let content = std::fs::read_to_string(path).map_err(|error| CliExit::error(error.to_string()))?;
  if content.trim().is_empty() {
    return Err(CliExit::usage(format!("{flag_name} is empty: {file_path}")));
  }
  Ok(content)
}

/// `read_reply_text`: stdin when the arg is `-`, else the file's text.
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
