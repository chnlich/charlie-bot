//! `parse_ncu_report`: the cache keyed by (absolute path, mtime in ns), and the choice of parser for each cache miss.
//!
//! Python imports `ncu_report` once per process and remembers the outcome (`_ncu_import_attempted`). Here the import
//! runs in the helper child, so the memo lives in `ModuleState`. `Undecided` runs the discovery and the first helper
//! call. `Available` keeps running the helper, and `Unavailable` keeps the CSV path for the rest of the process.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use charliebot_core::python_helper::PythonHelper;
use serde::Deserialize;
use serde_json::Value;

use crate::csv_import;
use crate::discovery::discover_python_dir;
use crate::error::ParseError;
use crate::pycompat;

/// The helper script, relative to the repo root. It imports `src.features.ncu.ncu_parsing` and keeps no copy of it.
const HELPER_SCRIPT: &str = "rust/helpers/ncu_report_helper.py";
/// Where `_discover_ncu_python_dir` globs for an install (`ncu_parsing.py:155`).
pub const NSIGHT_ROOT: &str = "/opt/nvidia/nsight-compute";

/// Whether `ncu_report` can be imported in this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModuleState {
  /// No import has been attempted. The first cache miss decides.
  Undecided,
  /// The helper imported the module on its last run.
  Available,
  /// No `extras/python` exists, or the helper's import failed. The CSV path serves every later miss.
  Unavailable,
}

/// (absolute path, mtime in nanoseconds), as `ncu_parsing.py:694` keys its cache.
type CacheKey = (String, i128);

/// The helper's reply, one of the three `status` values `ncu_report_helper.py` prints.
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum HelperReply {
  Ok {
    report: Value,
  },
  ModuleUnavailable {
    python_dir: Option<String>,
  },
  #[serde(rename = "parse_error")]
  Failed {
    error: String,
  },
}

/// The report cache and the import memo of one server.
pub struct Viewer {
  path_env: Option<OsString>,
  nsight_root: PathBuf,
  module: Mutex<ModuleState>,
  cache: Mutex<HashMap<CacheKey, Arc<Value>>>,
}

impl Viewer {
  /// A viewer that searches `path_env` for `ncu` and globs `nsight_root` for an install.
  pub fn new(path_env: Option<OsString>, nsight_root: PathBuf) -> Viewer {
    Viewer { path_env, nsight_root, module: Mutex::new(ModuleState::Undecided), cache: Mutex::new(HashMap::new()) }
  }

  /// `parse_ncu_report(path)`: the report dict for `path`, an absolute path as pathlib writes it. A cached report
  /// comes back as the same `Arc` while the file's mtime holds.
  pub fn parse(&self, repo_root: &Path, path: &str) -> Result<Arc<Value>, ParseError> {
    let abspath = pycompat::normpath(path);
    let key = (abspath.clone(), modification_ns(&abspath)?);
    if let Some(report) = self.cache.lock().expect("the cache lock is not poisoned").get(&key) {
      return Ok(Arc::clone(report));
    }

    let mut report = self.parse_uncached(repo_root, &abspath)?;
    let object = report.as_object_mut().expect("a parsed report is a JSON object");
    object.insert("path".to_string(), Value::String(abspath.clone()));
    object.insert("filename".to_string(), Value::String(pycompat::basename(&abspath).to_string()));
    let report = Arc::new(report);
    self.cache.lock().expect("the cache lock is not poisoned").insert(key, Arc::clone(&report));
    Ok(report)
  }

  fn parse_uncached(&self, repo_root: &Path, abspath: &str) -> Result<Value, ParseError> {
    let state = *self.module.lock().expect("the module lock is not poisoned");
    match state {
      ModuleState::Unavailable => self.parse_csv(abspath),
      ModuleState::Undecided => match discover_python_dir(self.path_env.as_deref(), &self.nsight_root) {
        Ok(Some(_)) => self.parse_with_helper(repo_root, abspath),
        Ok(None) => {
          self.set_module(ModuleState::Unavailable);
          self.parse_csv(abspath)
        }
        Err(error) => Err(ParseError::Internal { kind: "OSError", detail: error.to_string() }),
      },
      ModuleState::Available => self.parse_with_helper(repo_root, abspath),
    }
  }

  /// `_parse_with_module`, run in the helper child.
  fn parse_with_helper(&self, repo_root: &Path, abspath: &str) -> Result<Value, ParseError> {
    let value = self
      .helper(repo_root)
      .run(Path::new(HELPER_SCRIPT), &[OsStr::new(abspath)], None, None)
      .map_err(|error| ParseError::Helper(error.to_string()))?;
    let reply: HelperReply = serde_json::from_value(value)
      .map_err(|error| ParseError::Helper(format!("the Python helper replied with an unknown shape: {error}")))?;
    match reply {
      HelperReply::Ok { report } => {
        self.set_module(ModuleState::Available);
        Ok(report)
      }
      HelperReply::ModuleUnavailable { python_dir } => {
        tracing::warn!(python_dir = python_dir.as_deref(), "ncu_report_import_failed");
        self.set_module(ModuleState::Unavailable);
        self.parse_csv(abspath)
      }
      HelperReply::Failed { error } => {
        self.set_module(ModuleState::Available);
        Err(ParseError::Report(error))
      }
    }
  }

  /// The helper runner, with the viewer's `PATH` so the helper's own `ncu` discovery and its section import match ours.
  fn helper(&self, repo_root: &Path) -> PythonHelper {
    let helper = PythonHelper::new(repo_root);
    match &self.path_env {
      Some(path) => helper.env("PATH", path),
      None => helper,
    }
  }

  fn parse_csv(&self, abspath: &str) -> Result<Value, ParseError> {
    csv_import::parse(csv_import::run(abspath, self.path_env.as_deref())?)
  }

  fn set_module(&self, state: ModuleState) {
    *self.module.lock().expect("the module lock is not poisoned") = state;
  }
}

/// `os.stat(abspath).st_mtime_ns`, or the `Report not accessible` error `ncu_parsing.py:692` raises.
fn modification_ns(abspath: &str) -> Result<i128, ParseError> {
  match fs::metadata(abspath) {
    Ok(metadata) => Ok(i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())),
    Err(error) => {
      Err(ParseError::Report(format!("Report not accessible: {}", pycompat::os_error_text(&error, abspath))))
    }
  }
}

#[cfg(test)]
mod tests {
  use std::time::{Duration, SystemTime};

  use serde_json::json;

  use super::*;
  use crate::test_support::{Install, fake_install, module_report, repo_root, write_report};

  /// A checkout whose interpreter is a file that cannot run, so a helper start fails with `Start`.
  fn checkout_without_a_runnable_interpreter() -> tempfile::TempDir {
    let checkout = tempfile::tempdir().unwrap();
    let interpreter = checkout.path().join(".venv").join("bin").join("python");
    fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
    fs::write(&interpreter, "not executable\n").unwrap();
    fs::set_permissions(&interpreter, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    checkout
  }

  fn viewer(path_env: &OsStr) -> Viewer {
    let no_installs = tempfile::tempdir().unwrap();
    Viewer::new(Some(path_env.to_os_string()), no_installs.path().to_path_buf())
  }

  #[test]
  fn without_an_extras_python_directory_the_csv_path_runs_and_no_helper_starts() {
    let install = Install::without_module();
    let reports = tempfile::tempdir().unwrap();
    let path = write_report(reports.path(), "a.ncu-rep", "not read by the CSV path");
    // A helper start would fail here, so a parsed report shows that none started.
    let checkout = checkout_without_a_runnable_interpreter();
    let viewer = viewer(&install.path_env());

    let report = Arc::unwrap_or_clone(viewer.parse(checkout.path(), &path).unwrap());

    assert_eq!(report["parser"], "csv");
    assert_eq!(report["filename"], "a.ncu-rep");
    assert_eq!(*viewer.module.lock().unwrap(), ModuleState::Unavailable);
  }

  #[test]
  fn a_helper_that_reports_a_failed_import_keeps_the_csv_path_for_the_process() {
    let install = Install::with_failing_module();
    let reports = tempfile::tempdir().unwrap();
    let first = write_report(reports.path(), "a.ncu-rep", "");
    let second = write_report(reports.path(), "b.ncu-rep", "");
    let viewer = viewer(&install.path_env());

    let first_report = Arc::unwrap_or_clone(viewer.parse(&repo_root(), &first).unwrap());
    assert_eq!(first_report["parser"], "csv");
    assert_eq!(install.imports(), 1);

    let second_report = Arc::unwrap_or_clone(viewer.parse(&repo_root(), &second).unwrap());
    assert_eq!(second_report["parser"], "csv");
    assert_eq!(install.imports(), 1, "an uncached report after a failed import starts no second helper");
  }

  #[test]
  fn an_interpreter_that_cannot_start_is_a_helper_error() {
    let install = Install::with_failing_module();
    let reports = tempfile::tempdir().unwrap();
    let path = write_report(reports.path(), "a.ncu-rep", "");
    let checkout = checkout_without_a_runnable_interpreter();

    let error = viewer(&install.path_env()).parse(checkout.path(), &path).unwrap_err();

    let ParseError::Helper(text) = error else { panic!("expected a helper error, got {error:?}") };
    assert!(text.contains("did not start"), "{text}");
    assert_eq!(install.imports(), 0);
  }

  #[test]
  fn the_helper_parses_through_the_module_and_the_cache_returns_the_same_report() {
    let search_path = OsString::from(format!("{}/bin:/usr/bin:/bin", fake_install().display()));
    let reports = tempfile::tempdir().unwrap();
    let path = write_report(reports.path(), "a.ncu-rep", &module_report("synthetic_gemm"));
    let viewer = viewer(&search_path);

    let first = viewer.parse(&repo_root(), &path).unwrap();

    assert_eq!(first["parser"], "ncu_report");
    assert_eq!(first["kernels"][0]["name"], "synthetic_gemm");
    assert_eq!(first["kernels"][0]["duration_ns"], json!(1500.0));
    assert_eq!(first["path"], json!(path));
    assert_eq!(first["filename"], "a.ncu-rep");
    let second = viewer.parse(&repo_root(), &path).unwrap();
    assert!(Arc::ptr_eq(&first, &second), "a cache hit returns the stored report");
  }

  #[test]
  fn a_changed_mtime_parses_the_file_again() {
    let search_path = OsString::from(format!("{}/bin:/usr/bin:/bin", fake_install().display()));
    let reports = tempfile::tempdir().unwrap();
    let path = write_report(reports.path(), "a.ncu-rep", &module_report("before"));
    let viewer = viewer(&search_path);
    let first = viewer.parse(&repo_root(), &path).unwrap();

    fs::write(&path, module_report("after")).unwrap();
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(10)).unwrap();
    let second = viewer.parse(&repo_root(), &path).unwrap();

    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(second["kernels"][0]["name"], "after");
  }

  #[test]
  fn a_module_that_refuses_the_report_is_a_report_error() {
    let search_path = OsString::from(format!("{}/bin:/usr/bin:/bin", fake_install().display()));
    let reports = tempfile::tempdir().unwrap();
    let path = write_report(reports.path(), "a.ncu-rep", r#"{"refuse": "synthetic refusal", "kernels": []}"#);
    let viewer = viewer(&search_path);

    let error = viewer.parse(&repo_root(), &path).unwrap_err();

    let ParseError::Report(text) = error else { panic!("expected a report error, got {error:?}") };
    assert_eq!(text, "Unable to load report: synthetic refusal");
    assert_eq!(*viewer.module.lock().unwrap(), ModuleState::Available);
  }

  #[test]
  fn a_missing_report_is_not_accessible() {
    let reports = tempfile::tempdir().unwrap();
    let path = reports.path().join("missing.ncu-rep").display().to_string();
    let checkout = checkout_without_a_runnable_interpreter();

    let error = viewer(&OsString::from("/usr/bin:/bin")).parse(checkout.path(), &path).unwrap_err();

    let ParseError::Report(text) = error else { panic!("expected a report error, got {error:?}") };
    assert!(text.starts_with("Report not accessible: [Errno 2] No such file or directory: '"), "{text}");
  }
}
