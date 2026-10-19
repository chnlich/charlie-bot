//! Why a report did not render. The page answers each kind with its own status.

/// A failed parse.
#[derive(Debug)]
pub enum ParseError {
  /// Python's `NcuParseError`: the page shows the message with status 422.
  Report(String),
  /// The Python helper failed in a way its reply cannot describe: the page shows the text with status 500.
  Helper(String),
  /// An exception Python's viewer does not catch: the server answers a plain 500 and logs `kind`, the Python class.
  Internal { kind: &'static str, detail: String },
}
