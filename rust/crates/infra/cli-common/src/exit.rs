//! CLI exits that carry their own stderr object: the `exit_usage_error` / `exit_error` contract.

use serde_json::{Value, json};

/// One CLI failure: the JSON object for stderr and the process exit code.
///
/// A verb returns this instead of exiting, so library tests can assert on the object and code the
/// way the Python tests catch `SystemExit`; the binary's `main` prints the body one-line and exits.
#[derive(Debug, Clone, PartialEq)]
pub struct CliExit {
  pub code: i32,
  pub body: Value,
}

impl CliExit {
  /// `exit_usage_error`: `{"error": <message>}` on stderr, exit 2 — argparse-compatible.
  pub fn usage(message: impl Into<String>) -> CliExit {
    CliExit { code: 2, body: json!({ "error": message.into() }) }
  }

  /// `exit_error`: the failure contract's JSON object, exit 1.
  pub fn error(message: impl Into<String>) -> CliExit {
    CliExit { code: 1, body: json!({ "error": message.into() }) }
  }

  /// A failure object that already carries its `code`/`effect` fields (the client contract's shapes).
  pub fn contract(error: Value, code: i32) -> CliExit {
    CliExit { code, body: error }
  }
}
