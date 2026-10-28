//! The items every switch helper shares: the group's usage line, the argument shapes, the failure code and the raw
//! file reads. No module here reads another module of this crate, so every helper can use them.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use anyhow::Context as _;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config::{self, CONFIG_FILENAME};
use charliebot_core::pyfmt;
use charliebot_store::metadata::MetadataReadError;
use serde_json::Value;

/// The group's usage line, printed for `--help` and for a bad name or argument count.
pub(crate) const USAGE: &str = "usage: charliebot switch-helper <name> [args...]";

/// Exactly `N` positional arguments, as text; any other count is a usage error.
pub(crate) fn exact<const N: usize>(args: &[OsString]) -> Result<[String; N], CliExit> {
  let values = args
    .iter()
    .map(|arg| arg.to_str().map(str::to_string).ok_or_else(|| CliExit::usage(USAGE)))
    .collect::<Result<Vec<_>, _>>()?;
  values.try_into().map_err(|_| CliExit::usage(USAGE))
}

/// The failure of a helper's work: a broken file or a bad record exits 1, as Python's uncaught exception does.
pub(crate) fn failed(error: impl std::fmt::Display) -> CliExit {
  CliExit::error(error.to_string())
}

/// Python truthiness of a value `serde_json` parsed: `None`, `False`, `0`, `""` and empty containers are false.
pub(crate) fn python_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(value) => *value,
    Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
    Value::String(value) => !value.is_empty(),
    Value::Array(value) => !value.is_empty(),
    Value::Object(value) => !value.is_empty(),
  }
}

/// Python's `repr(value)` for a value `json` parsed: strings are quoted, `None` prints bare.
pub(crate) fn python_repr(value: &Value) -> String {
  match value {
    Value::String(text) => pyfmt::str_repr(text),
    other => pyfmt::python_str(other),
  }
}

/// A helper that reports a plain exit code instead of a `CliExit` body, for the polled helpers whose no-match is
/// silent. `code` is 0 or 1.
pub(crate) fn exit_with(code: i32) -> ! {
  std::process::exit(code)
}

/// One JSON document, as Python's `json.load` reads it.
pub(crate) fn read_json(path: &Path) -> anyhow::Result<Value> {
  let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// The failure text of a metadata read: the verb names the step that failed, as `read_json` reports it. An
/// unreadable file reads `read <path>` and a file that is not JSON reads `parse <path>`.
pub(crate) fn metadata_read_failure(path: &Path, error: MetadataReadError) -> String {
  match error {
    MetadataReadError::Io(_) => format!("read {}", path.display()),
    MetadataReadError::Json(_) => format!("parse {}", path.display()),
  }
}

/// The profile home's `config.yaml` as a JSON value. The switch reads the raw file rather than the loaded
/// `CharlieBotConfig`: the scan reports a backend option whose type no package registers, and the loader refuses such
/// a config instead.
pub(crate) fn read_config(home: &Path) -> anyhow::Result<Value> {
  Ok(serde_json::to_value(config::read_yaml_document(&home.join(CONFIG_FILENAME))?)?)
}
