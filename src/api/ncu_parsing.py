"""Parse Nsight Compute (.ncu-rep) reports into a JSON-serializable overview.

Primary path uses the `ncu_report` Python module that ships with the system
Nsight Compute install. If that module cannot be imported, we fall back to
`ncu --import <file> --csv --page details`. Parsed results are cached keyed by
(absolute path, st_mtime_ns) and reparsed when the file changes on disk.
"""

import csv
import glob
import io
import math
import os
import shutil
import subprocess
import sys
from pathlib import Path

import structlog

from src.core.timeouts import SUBPROCESS_NCU_CSV_IMPORT_TIMEOUT

log = structlog.get_logger()


class NcuParseError(Exception):
  """Raised when a report is missing, invalid, or cannot be parsed."""


# Metric names surfaced as top-line summary columns. Any that were not
# collected resolve to None and render as N/A in the page.
_DURATION_METRIC = "gpu__time_duration.sum"
_OCCUPANCY_METRIC = "sm__warps_active.avg.pct_of_peak_sustained_active"
_SOL_COMPUTE_METRIC = "sm__throughput.avg.pct_of_peak_sustained_elapsed"
_SOL_MEMORY_METRIC = "gpu__compute_memory_throughput.avg.pct_of_peak_sustained_elapsed"
_DEVICE_METRIC = "device__attribute_display_name"

_TIME_UNIT_TO_NS = {
    "ns": 1.0,
    "nsecond": 1.0,
    "us": 1e3,
    "usecond": 1e3,
    "ms": 1e6,
    "msecond": 1e6,
    "s": 1e9,
    "second": 1e9,
}

# (absolute path, st_mtime_ns) -> parsed report dict.
_CACHE: dict[tuple[str, int], dict] = {}

_ncu_report_module = None
_ncu_import_attempted = False


def _discover_ncu_python_dir() -> str | None:
  """Locate the Nsight Compute `extras/python` dir containing ncu_report.py.

  Resolves the `ncu` binary and walks up looking for extras/python, then falls
  back to globbing the standard install root and picking the newest version.
  """
  ncu = shutil.which("ncu")
  if ncu:
    for parent in Path(ncu).resolve().parents:
      candidate = parent / "extras" / "python"
      if (candidate / "ncu_report.py").is_file():
        return str(candidate)

  globbed = sorted(glob.glob("/opt/nvidia/nsight-compute/*/extras/python"))
  for candidate in reversed(globbed):
    if (Path(candidate) / "ncu_report.py").is_file():
      return candidate

  return None


def _load_ncu_report_module():
  """Import and memoize the ncu_report module, or None if unavailable."""
  global _ncu_report_module, _ncu_import_attempted
  if _ncu_import_attempted:
    return _ncu_report_module

  _ncu_import_attempted = True
  python_dir = _discover_ncu_python_dir()
  if python_dir and python_dir not in sys.path:
    sys.path.append(python_dir)

  try:
    import ncu_report  # type: ignore

    _ncu_report_module = ncu_report
  except Exception:
    log.warning("ncu_report_import_failed", python_dir=python_dir)
    _ncu_report_module = None
  return _ncu_report_module


def _clean_number(value):
  """Replace non-finite floats with None so the result is JSON-safe."""
  if isinstance(value, float) and not math.isfinite(value):
    return None
  return value


def _metric_value(action, name: str) -> tuple[object, str | None]:
  """Return (value, unit) for a metric, or (None, None) if not collected.

  Handles both a missing metric (metric_by_name returns None) and a metric that
  is present but has no value, without raising.
  """
  metric = action.metric_by_name(name)
  if metric is None:
    return None, None
  try:
    if hasattr(metric, "has_value") and not metric.has_value():
      return None, None
    value = _clean_number(metric.value())
    unit = metric.unit() or None
    return value, unit
  except Exception:
    return None, None


def _dims(action, prefix: str) -> str | None:
  """Format a 3D launch dimension (e.g. '64 × 6 × 1'), or None if absent."""
  x = _metric_value(action, f"{prefix}_x")[0]
  y = _metric_value(action, f"{prefix}_y")[0]
  z = _metric_value(action, f"{prefix}_z")[0]
  if x is None:
    return None
  parts = [x, 1 if y is None else y, 1 if z is None else z]
  return " × ".join(str(int(p)) if isinstance(p, float) and p.is_integer() else str(p) for p in parts)


def _duration_ns(value, unit: str | None):
  """Normalize a duration metric to nanoseconds for sorting/display."""
  if value is None or not isinstance(value, (int, float)):
    return None
  factor = _TIME_UNIT_TO_NS.get((unit or "ns").strip().lower(), 1.0)
  return float(value) * factor


def _action_to_dict(action, idx: int) -> dict:
  """Build the per-kernel record, including every collected metric."""
  dur_value, dur_unit = _metric_value(action, _DURATION_METRIC)
  metrics: list[dict] = []
  for name in action.metric_names():
    value, unit = _metric_value(action, name)
    if value is None:
      continue
    metrics.append({"name": name, "value": value, "unit": unit or ""})

  return {
      "idx": idx,
      "name": action.name(),
      "duration_ns": _duration_ns(dur_value, dur_unit),
      "grid": _dims(action, "launch__grid_dim"),
      "block": _dims(action, "launch__block_dim"),
      "occupancy_pct": _metric_value(action, _OCCUPANCY_METRIC)[0],
      "sol_compute_pct": _metric_value(action, _SOL_COMPUTE_METRIC)[0],
      "sol_memory_pct": _metric_value(action, _SOL_MEMORY_METRIC)[0],
      "metrics": metrics,
  }


def _parse_with_module(module, abspath: str) -> dict:
  """Parse a report using the ncu_report module."""
  try:
    ctx = module.load_report(abspath)
  except Exception as exc:
    raise NcuParseError(f"Unable to load report: {exc}") from exc

  kernels: list[dict] = []
  device: str | None = None
  for i in range(ctx.num_ranges()):
    rng = ctx.range_by_idx(i)
    for j in range(rng.num_actions()):
      action = rng.action_by_idx(j)
      kernels.append(_action_to_dict(action, len(kernels)))
      if device is None:
        device_value = _metric_value(action, _DEVICE_METRIC)[0]
        if isinstance(device_value, str):
          device = device_value

  if not kernels:
    raise NcuParseError("Report contains no profiled kernels.")

  return {"kernels": kernels, "device": device, "parser": "ncu_report"}


def _format_dims_csv(raw: str) -> str | None:
  """Convert a CSV launch dim like '(64, 6, 1)' to '64 × 6 × 1'."""
  raw = (raw or "").strip().strip("()")
  if not raw:
    return None
  parts = [p.strip() for p in raw.split(",") if p.strip()]
  return " × ".join(parts) if parts else None


def _coerce_csv_value(raw: str):
  """Convert a CSV metric value string to int/float when numeric."""
  text = (raw or "").strip().replace(",", "")
  if not text:
    return None
  try:
    if text.lstrip("-").isdigit():
      return int(text)
    return _clean_number(float(text))
  except ValueError:
    return raw


def _parse_with_csv(abspath: str) -> dict:
  """Fallback parser: drive `ncu --import ... --csv --page details`."""
  cmd = ["ncu", "--import", abspath, "--csv", "--page", "details"]
  try:
    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        timeout=SUBPROCESS_NCU_CSV_IMPORT_TIMEOUT,
    )
  except FileNotFoundError as exc:
    raise NcuParseError("ncu_report module unavailable and the `ncu` binary was not found.") from exc
  except subprocess.TimeoutExpired as exc:
    raise NcuParseError("ncu CSV import timed out.") from exc

  if proc.returncode != 0:
    detail = (proc.stderr or proc.stdout).strip()[:500]
    raise NcuParseError(f"ncu CSV import failed: {detail}")

  reader = csv.DictReader(io.StringIO(proc.stdout))
  by_id: dict[str, dict] = {}
  order: list[str] = []
  for row in reader:
    action_id = row.get("ID")
    if action_id is None:
      continue
    if action_id not in by_id:
      order.append(action_id)
      by_id[action_id] = {
          "name": row.get("Kernel Name", ""),
          "grid": _format_dims_csv(row.get("Grid Size", "")),
          "block": _format_dims_csv(row.get("Block Size", "")),
          "metrics": {},
      }
    name = row.get("Metric Name")
    if name:
      by_id[action_id]["metrics"][name] = (
          _coerce_csv_value(row.get("Metric Value", "")),
          (row.get("Metric Unit") or "").strip(),
      )

  if not order:
    raise NcuParseError("ncu CSV import produced no kernels.")

  kernels: list[dict] = []
  for idx, action_id in enumerate(order):
    entry = by_id[action_id]
    raw_metrics = entry["metrics"]
    dur_value, dur_unit = raw_metrics.get(_DURATION_METRIC, (None, None))
    metrics = [{"name": n, "value": v, "unit": u} for n, (v, u) in raw_metrics.items() if v is not None]
    kernels.append({
        "idx": idx,
        "name": entry["name"],
        "duration_ns": _duration_ns(dur_value, dur_unit),
        "grid": entry["grid"],
        "block": entry["block"],
        "occupancy_pct": raw_metrics.get(_OCCUPANCY_METRIC, (None, None))[0],
        "sol_compute_pct": raw_metrics.get(_SOL_COMPUTE_METRIC, (None, None))[0],
        "sol_memory_pct": raw_metrics.get(_SOL_MEMORY_METRIC, (None, None))[0],
        "metrics": metrics,
    })

  return {"kernels": kernels, "device": None, "parser": "csv"}


def parse_ncu_report(path: str) -> dict:
  """Parse a .ncu-rep report into a JSON-serializable overview dict.

  Results are cached by (absolute path, st_mtime_ns). Raises NcuParseError on a
  missing, invalid, or unparseable report.
  """
  abspath = os.path.abspath(path)
  try:
    mtime_ns = os.stat(abspath).st_mtime_ns
  except OSError as exc:
    raise NcuParseError(f"Report not accessible: {exc}") from exc

  key = (abspath, mtime_ns)
  cached = _CACHE.get(key)
  if cached is not None:
    return cached

  module = _load_ncu_report_module()
  if module is not None:
    report = _parse_with_module(module, abspath)
  else:
    report = _parse_with_csv(abspath)

  report["path"] = abspath
  report["filename"] = os.path.basename(abspath)
  _CACHE[key] = report
  return report
