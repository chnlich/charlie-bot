//! The native fallback parser: `ncu --import <file> --csv --page details`, read as `_parse_with_csv` reads it.
//!
//! The CSV path has no rules, SASS, PTX or CUDA source. Its kernels carry the metrics, the sections and the roofline.
//! `run` starts the process; `parse` reads its outcome. The differential compares `parse` with Python's
//! `_parse_with_csv` over canned outputs.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::process::{Command, Stdio};
use std::time::Duration;

use charliebot_core::python_helper::{Capture, capture};
use serde_json::{Number, Value, json};

use crate::csv_reader::{Row, dict_rows};
use crate::error::ParseError;
use crate::pycompat::{strip, take_chars};
use crate::roofline;

/// `SUBPROCESS_NCU_CSV_IMPORT_TIMEOUT` (`ncu_parsing.py:37`), the bound on the `ncu` run.
const IMPORT_TIMEOUT: Duration = Duration::from_secs(120);
/// The `_parse_with_csv` message for a missing `ncu` (`ncu_parsing.py:606`).
const NCU_NOT_FOUND: &str = "ncu_report module unavailable and the `ncu` binary was not found.";
/// The longest stderr or stdout that the failure message quotes (`ncu_parsing.py:611`), in code points.
const DETAIL_LIMIT: usize = 500;

const DURATION_METRIC: &str = "gpu__time_duration.sum";
const OCCUPANCY_METRIC: &str = "sm__warps_active.avg.pct_of_peak_sustained_active";
const SOL_COMPUTE_METRIC: &str = "sm__throughput.avg.pct_of_peak_sustained_elapsed";
const SOL_MEMORY_METRIC: &str = "gpu__compute_memory_throughput.avg.pct_of_peak_sustained_elapsed";

/// What `ncu --import <file> --csv --page details` did, as `_parse_with_csv` sees it.
#[derive(Debug)]
pub enum Outcome {
  /// The process ran to its end. `success` is `returncode == 0`. The text is decoded with universal newlines.
  Completed { success: bool, stdout: String, stderr: String },
  /// No `ncu` on the search path (`FileNotFoundError`).
  NotFound,
  /// The process ran past its timeout (`TimeoutExpired`).
  TimedOut,
}

/// `_run_ncu_csv_import`: run `ncu --import <abspath> --csv --page details` with `path_env` as its `PATH`.
pub fn run(abspath: &str, path_env: Option<&OsStr>) -> Result<Outcome, ParseError> {
  let mut command = Command::new("ncu");
  command.args(["--import", abspath, "--csv", "--page", "details"]).stdout(Stdio::piped()).stderr(Stdio::piped());
  if let Some(path) = path_env {
    command.env("PATH", path);
  }
  let child = match command.spawn() {
    Ok(child) => child,
    Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Outcome::NotFound),
    Err(error) => return Err(ParseError::Internal { kind: "OSError", detail: error.to_string() }),
  };
  match capture(child, None, Some(IMPORT_TIMEOUT)) {
    Ok(Capture::TimedOut { .. }) => Ok(Outcome::TimedOut),
    Ok(Capture::Exited(output)) => Ok(Outcome::Completed {
      success: output.status.success(),
      stdout: decode(output.stdout)?,
      stderr: decode(output.stderr)?,
    }),
    Err(error) => Err(ParseError::Internal { kind: "OSError", detail: error.to_string() }),
  }
}

/// `text=True` decoding: strict UTF-8, then universal newlines.
fn decode(bytes: Vec<u8>) -> Result<String, ParseError> {
  let text = String::from_utf8(bytes)
    .map_err(|error| ParseError::Internal { kind: "UnicodeDecodeError", detail: error.to_string() })?;
  Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// `_parse_with_csv`'s body, from the outcome of the `ncu` run to the report dict without `path` and `filename`.
pub fn parse(outcome: Outcome) -> Result<Value, ParseError> {
  match outcome {
    Outcome::NotFound => Err(ParseError::Report(NCU_NOT_FOUND.to_string())),
    Outcome::TimedOut => Err(ParseError::Report("ncu CSV import timed out.".to_string())),
    Outcome::Completed { success: false, stdout, stderr } => {
      let text = if stderr.is_empty() { &stdout } else { &stderr };
      let detail = take_chars(strip(text), DETAIL_LIMIT);
      Err(ParseError::Report(format!("ncu CSV import failed: {detail}")))
    }
    Outcome::Completed { success: true, stdout, .. } => parse_csv(&stdout),
  }
}

/// One kernel, from the first row that names its ID. Its metrics take every later row of that ID.
struct Kernel {
  name: Option<String>,
  grid: Option<String>,
  block: Option<String>,
  cc: String,
  metrics: Metrics,
}

impl Kernel {
  fn from_row(row: &Row) -> Kernel {
    Kernel {
      name: row.get("Kernel Name").unwrap_or(Some("")).map(str::to_string),
      grid: format_dims(row.get("Grid Size").flatten().unwrap_or("")),
      block: format_dims(row.get("Block Size").flatten().unwrap_or("")),
      cc: strip(row.get("CC").flatten().unwrap_or("")).to_string(),
      metrics: Metrics::default(),
    }
  }
}

/// A coerced metric value and its unit.
struct Metric {
  value: Cell,
  unit: String,
}

/// `raw_metrics`: a dict in insertion order. A repeated name keeps its first position and takes the last value.
#[derive(Default)]
struct Metrics {
  entries: Vec<(String, Metric)>,
  positions: HashMap<String, usize>,
}

impl Metrics {
  fn insert(&mut self, name: &str, metric: Metric) {
    match self.positions.get(name) {
      Some(&position) => self.entries[position].1 = metric,
      None => {
        self.positions.insert(name.to_string(), self.entries.len());
        self.entries.push((name.to_string(), metric));
      }
    }
  }

  fn get(&self, name: &str) -> Option<&Metric> {
    self.positions.get(name).map(|&position| &self.entries[position].1)
  }

  /// `raw_metrics.get(name, (None, None))[0]`: the value, or null when the metric was not collected.
  fn value_json(&self, name: &str) -> Value {
    self.get(name).map_or(Value::Null, |metric| metric.value.to_value())
  }
}

/// A `_coerce_csv_value` result: an integer, a float, the raw text when neither parses, or nothing.
#[derive(Clone, Debug, PartialEq)]
enum Cell {
  Null,
  Int(Number),
  Float(f64),
  Text(String),
}

impl Cell {
  fn to_value(&self) -> Value {
    match self {
      Cell::Null => Value::Null,
      Cell::Int(value) => Value::Number(value.clone()),
      Cell::Float(value) => serde_json::Number::from_f64(*value).map_or(Value::Null, Value::Number),
      Cell::Text(text) => Value::String(text.clone()),
    }
  }
}

/// `_group_rows_into_sections`: each kernel's sections, in first-seen order, indexed by the kernel's position.
struct Section {
  name: String,
  metrics: Vec<Value>,
}

fn group_rows_into_sections(rows: &[Row]) -> Result<Vec<Vec<Value>>, ParseError> {
  let mut kernels: Vec<Vec<Section>> = Vec::new();
  let mut positions: HashMap<&str, usize> = HashMap::new();
  for row in rows {
    let Some(action_id) = row.get("ID").flatten() else {
      continue;
    };
    let position = *positions.entry(action_id).or_insert_with(|| {
      kernels.push(Vec::new());
      kernels.len() - 1
    });
    let section_name = match strip(row.get("Section Name").flatten().unwrap_or("")) {
      "" => "Other metrics",
      name => name,
    };
    let sections = &mut kernels[position];
    let section = match sections.iter().position(|section| section.name == section_name) {
      Some(index) => index,
      None => {
        sections.push(Section { name: section_name.to_string(), metrics: Vec::new() });
        sections.len() - 1
      }
    };
    if let Some(name) = row.get("Metric Name").flatten().filter(|name| !name.is_empty()) {
      let value = coerce(row.get("Metric Value").flatten())?;
      sections[section].metrics.push(json!({
        "name": name,
        "value": value.to_value(),
        "unit": strip(row.get("Metric Unit").flatten().unwrap_or("")),
      }));
    }
  }
  Ok(
    kernels
      .into_iter()
      .map(|sections| {
        sections.into_iter().map(|section| json!({"name": section.name, "metrics": section.metrics})).collect()
      })
      .collect(),
  )
}

fn parse_csv(stdout: &str) -> Result<Value, ParseError> {
  let rows =
    dict_rows(stdout).map_err(|error| ParseError::Internal { kind: "_csv.Error", detail: error.to_string() })?;
  let mut kernels: Vec<Kernel> = Vec::new();
  let mut positions: HashMap<&str, usize> = HashMap::new();
  for row in &rows {
    let Some(action_id) = row.get("ID").flatten() else {
      continue;
    };
    let position = match positions.get(action_id) {
      Some(&position) => position,
      None => {
        positions.insert(action_id, kernels.len());
        kernels.push(Kernel::from_row(row));
        kernels.len() - 1
      }
    };
    if let Some(name) = row.get("Metric Name").flatten().filter(|name| !name.is_empty()) {
      let metric = Metric {
        value: coerce(row.get("Metric Value").flatten())?,
        unit: strip(row.get("Metric Unit").flatten().unwrap_or("")).to_string(),
      };
      kernels[position].metrics.insert(name, metric);
    }
  }
  if kernels.is_empty() {
    return Err(ParseError::Report("ncu CSV import produced no kernels.".to_string()));
  }

  let sections = group_rows_into_sections(&rows)?;
  let mut rendered = Vec::with_capacity(kernels.len());
  for (idx, kernel) in kernels.iter().enumerate() {
    let duration_ns = match kernel.metrics.get(DURATION_METRIC) {
      Some(metric) => duration_ns(&metric.value, &metric.unit),
      None => None,
    };
    let metrics: Vec<Value> = kernel
      .metrics
      .entries
      .iter()
      .filter(|(_, metric)| metric.value != Cell::Null)
      .map(|(name, metric)| json!({"name": name, "value": metric.value.to_value(), "unit": metric.unit}))
      .collect();
    rendered.push(json!({
      "idx": idx,
      "name": kernel.name,
      "duration_ns": duration_ns,
      "grid": kernel.grid,
      "block": kernel.block,
      "occupancy_pct": kernel.metrics.value_json(OCCUPANCY_METRIC),
      "sol_compute_pct": kernel.metrics.value_json(SOL_COMPUTE_METRIC),
      "sol_memory_pct": kernel.metrics.value_json(SOL_MEMORY_METRIC),
      "metrics": metrics,
      "rules": [],
      "sections": sections[idx],
      "sass": [],
      "ptx": [],
      "cuda_sources": [],
    }));
  }

  let first_cc = &kernels[0].cc;
  let device_summary =
    if first_cc.is_empty() { Vec::new() } else { vec![json!({"label": "Compute capability", "value": first_cc})] };
  let roofline = roofline::collect(&rendered)?;
  Ok(json!({
    "kernels": rendered,
    "device": null,
    "parser": "csv",
    "device_attributes": [],
    "device_summary": device_summary,
    "roofline": roofline,
  }))
}

/// `_coerce_csv_value`. Commas are thousands separators and go first. An integer is an optional minus and digits, a
/// float is anything `float()` takes that is finite, and the raw text is what remains.
fn coerce(raw: Option<&str>) -> Result<Cell, ParseError> {
  let raw_text = raw.unwrap_or("");
  let text = strip(raw_text).replace(',', "");
  if text.is_empty() {
    return Ok(Cell::Null);
  }
  let unsigned = text.trim_start_matches('-');
  if !unsigned.is_empty() && unsigned.bytes().all(|byte| byte.is_ascii_digit()) {
    // `int(text)` takes one minus sign. A second one raises ValueError, and the raw text stands.
    if text.len() - unsigned.len() > 1 {
      return Ok(Cell::Text(raw_text.to_string()));
    }
    // Normalize leading zeroes because Python accepts them in `int`, while JSON numbers do not.
    let negative = text.starts_with('-');
    let digits = unsigned.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let canonical = if negative && digits != "0" { format!("-{digits}") } else { digits.to_string() };
    let value: Value = serde_json::from_str(&canonical).expect("a canonical integer is valid JSON");
    return Ok(Cell::Int(value.as_number().expect("a canonical integer is a JSON number").clone()));
  }
  match text.parse::<f64>() {
    Ok(value) if value.is_finite() => Ok(Cell::Float(value)),
    Ok(_) => Ok(Cell::Null),
    Err(_) => Ok(Cell::Text(raw_text.to_string())),
  }
}

/// `_format_dims_csv`: `(64, 6, 1)` becomes `64 × 6 × 1`, and nothing when no part remains.
fn format_dims(raw: &str) -> Option<String> {
  let inner = strip(raw).trim_matches(|c| c == '(' || c == ')');
  let parts: Vec<&str> = inner.split(',').map(strip).filter(|part| !part.is_empty()).collect();
  (!parts.is_empty()).then(|| parts.join(" × "))
}

/// `_duration_ns`: a numeric metric in nanoseconds. A unit that is not a known time unit counts as nanoseconds.
fn duration_ns(value: &Cell, unit: &str) -> Option<f64> {
  let number = match value {
    Cell::Int(value) => value.as_f64()?,
    Cell::Float(value) => *value,
    Cell::Null | Cell::Text(_) => return None,
  };
  let unit = strip(unit);
  let unit = if unit.is_empty() { "ns" } else { unit };
  let factor = match unit.to_lowercase().as_str() {
    "ns" | "nsecond" => 1.0,
    "us" | "usecond" => 1e3,
    "ms" | "msecond" => 1e6,
    "s" | "second" => 1e9,
    _ => 1.0,
  };
  Some(number * factor)
}

#[cfg(test)]
mod tests {
  use super::*;

  const TWO_KERNELS: &str = include_str!("../tests/canned/two_kernels.csv");
  const QUOTING: &str = include_str!("../tests/canned/quoting.csv");
  const BAD_NEWLINE: &str = include_str!("../tests/canned/bad_newline.csv");

  fn completed(stdout: &str) -> Outcome {
    Outcome::Completed { success: true, stdout: stdout.to_string(), stderr: String::new() }
  }

  fn report_error(outcome: Outcome) -> String {
    match parse(outcome) {
      Err(ParseError::Report(message)) => message,
      other => panic!("expected a report error, got {other:?}"),
    }
  }

  #[test]
  fn two_kernels_parse_into_metrics_sections_and_a_roofline_point() {
    let report = parse(completed(TWO_KERNELS)).unwrap();

    assert_eq!(report["parser"], "csv");
    assert_eq!(report["device"], Value::Null);
    assert_eq!(report["device_summary"], json!([{"label": "Compute capability", "value": "8.6"}]));
    let kernels = report["kernels"].as_array().unwrap();
    assert_eq!(kernels.len(), 2);
    let first = &kernels[0];
    assert_eq!(first["name"], "synthetic_gemm");
    assert_eq!(first["grid"], "64 × 6 × 1");
    assert_eq!(first["block"], "128 × 1 × 1");
    assert_eq!(first["duration_ns"], json!(1234.5 * 1e3));
    assert_eq!(first["occupancy_pct"], json!(41.25));
    assert_eq!(first["sol_compute_pct"], json!("n/a"));
    assert_eq!(first["sol_memory_pct"], json!(55.5));
    assert_eq!(first["sections"][0]["name"], "GPU Speed Of Light Throughput");
    assert_eq!(
      first["sections"][0]["metrics"][0],
      json!({"name": "gpu__time_duration.sum", "value": 1234.5, "unit": "usecond"})
    );
    assert_eq!(first["metrics"].as_array().unwrap().len(), 14);
    assert_eq!(report["roofline"]["available"], json!(true));
    assert_eq!(report["roofline"]["rooflines"][0]["points"].as_array().unwrap().len(), 1);
    assert_eq!(kernels[1]["name"], "synthetic_copy");
    assert_eq!(kernels[1]["duration_ns"], json!(7.0 * 1e6));
  }

  #[test]
  fn quoted_fields_keep_their_commas_newlines_and_quotes() {
    let report = parse(completed(QUOTING)).unwrap();

    let kernel = &report["kernels"][0];
    assert_eq!(kernel["name"], "quote, \"named\"\nkernel");
    assert_eq!(kernel["metrics"][0]["value"], json!(1234));
    assert_eq!(kernel["metrics"][1]["value"], json!("text kept"));
  }

  #[test]
  fn a_failed_run_quotes_stderr_truncated_to_500_code_points() {
    let stderr = "é".repeat(600);

    let message = report_error(Outcome::Completed { success: false, stdout: String::new(), stderr });

    assert_eq!(message, format!("ncu CSV import failed: {}", "é".repeat(500)));
  }

  #[test]
  fn a_failed_run_with_empty_stderr_quotes_stdout() {
    let message = report_error(Outcome::Completed {
      success: false,
      stdout: "  partial output \n".to_string(),
      stderr: String::new(),
    });

    assert_eq!(message, "ncu CSV import failed: partial output");
  }

  #[test]
  fn missing_ncu_and_a_timeout_have_their_messages() {
    assert_eq!(report_error(Outcome::NotFound), NCU_NOT_FOUND);
    assert_eq!(report_error(Outcome::TimedOut), "ncu CSV import timed out.");
  }

  #[test]
  fn output_without_kernels_is_a_report_error() {
    assert_eq!(report_error(completed("ID,Kernel Name\n")), "ncu CSV import produced no kernels.");
    assert_eq!(report_error(completed("")), "ncu CSV import produced no kernels.");
  }

  #[test]
  fn a_carriage_return_inside_an_unquoted_field_is_an_internal_error() {
    let error = parse(completed(BAD_NEWLINE)).unwrap_err();

    assert!(matches!(error, ParseError::Internal { kind: "_csv.Error", .. }), "{error:?}");
  }

  #[test]
  fn coercion_follows_python_int_then_float_then_raw_text() {
    assert_eq!(coerce(Some(" 1,024 ")).unwrap().to_value(), json!(1024));
    assert_eq!(coerce(Some("-7")).unwrap().to_value(), json!(-7));
    assert_eq!(coerce(Some("--7")).unwrap(), Cell::Text("--7".to_string()));
    assert_eq!(coerce(Some("2.5e3")).unwrap(), Cell::Float(2500.0));
    assert_eq!(coerce(Some("nan")).unwrap(), Cell::Null);
    assert_eq!(coerce(Some(" n/a ")).unwrap(), Cell::Text(" n/a ".to_string()));
    assert_eq!(coerce(None).unwrap(), Cell::Null);
    assert_eq!(
      coerce(Some("9223372036854775808")).unwrap().to_value(),
      json!(9_223_372_036_854_775_808_u64)
    );
  }

  #[test]
  fn dims_and_time_units_follow_the_python_helpers() {
    assert_eq!(format_dims(" (64, 6, 1) "), Some("64 × 6 × 1".to_string()));
    assert_eq!(format_dims("()"), None);
    assert_eq!(duration_ns(&Cell::Int(Number::from(3)), "MSecond"), Some(3e6));
    assert_eq!(duration_ns(&Cell::Float(2.0), "furlong"), Some(2.0));
    assert_eq!(duration_ns(&Cell::Text("slow".to_string()), "s"), None);
  }
}
