"""Parse Nsight Compute (.ncu-rep) reports into a JSON-serializable overview.

Primary path uses the `ncu_report` Python module that ships with the system
Nsight Compute install. If that module cannot be imported, we fall back to
`ncu --import <file> --csv --page details`. Parsed results are cached keyed by
(absolute path, st_mtime_ns) and reparsed when the file changes on disk.

Beyond the flat per-kernel metric set the report also surfaces, when present:
  * rule recommendations (the green/yellow advice the ncu UI shows),
  * metrics grouped by their ncu Section (Speed Of Light, Memory Workload, ...),
  * roofline chart points derived from NCU SpeedOfLight roofline metrics,
  * SASS disassembly plus PTX and CUDA-C source listings,
  * device / session attributes.
Every one of these is optional — a narrow ``--metrics`` capture carries almost
none of them — so each is collected best-effort and resolves to an empty value
the page renders as "not available" rather than raising.
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

# Function entry program counters, used to walk SASS/PTX listings by PC.
_FUNCTION_PCS_METRIC = "launch__function_pcs"

# NCU's overview roofline chart definitions from SpeedOfLight_RooflineChart.
# These names are the metric definitions Nsight Compute itself emits in
# --set full/detailed reports; narrow --metrics captures usually lack them.
_ROOFLINE_PEAK_SM_CLOCK = "sm__cycles_elapsed.avg.per_second"
_ROOFLINE_ACHIEVED_SM_CLOCK = "smsp__cycles_elapsed.avg.per_second"
_ROOFLINE_PEAK_TRAFFIC_PER_CYCLE = "dram__bytes.sum.peak_sustained"
_ROOFLINE_ACHIEVED_TRAFFIC = "dram__bytes.sum.per_second"
_ROOFLINE_DRAM_CLOCK = "dram__cycles_elapsed.avg.per_second"
_ROOFLINE_PLACEHOLDER = "本报告未采集 roofline 数据 (需要 --set full/detailed)"

_ROOFLINE_DEFINITIONS = [
    {
        "id": "fp32",
        "label": "Single Precision Roofline",
        "precision": "FP32",
        "peak_work_metric": "derived__sm__sass_thread_inst_executed_op_ffma_pred_on_x2",
        "required_achieved_work_metrics": [
            "smsp__sass_thread_inst_executed_op_fadd_pred_on.sum.per_cycle_elapsed",
            "smsp__sass_thread_inst_executed_op_fmul_pred_on.sum.per_cycle_elapsed",
            "derived__smsp__sass_thread_inst_executed_op_ffma_pred_on_x2",
        ],
        "optional_achieved_work_metrics": [
            "derived__smsp__sass_thread_inst_executed_op_fadd2_pred_on_x2",
            "derived__smsp__sass_thread_inst_executed_op_fmul2_pred_on_x2",
            "derived__smsp__sass_thread_inst_executed_op_ffma2_pred_on_x4",
        ],
    },
    {
        "id": "fp64",
        "label": "Double Precision Roofline",
        "precision": "FP64",
        "peak_work_metric": "derived__sm__sass_thread_inst_executed_op_dfma_pred_on_x2",
        "required_achieved_work_metrics": [
            "smsp__sass_thread_inst_executed_op_dadd_pred_on.sum.per_cycle_elapsed",
            "smsp__sass_thread_inst_executed_op_dmul_pred_on.sum.per_cycle_elapsed",
            "derived__smsp__sass_thread_inst_executed_op_dfma_pred_on_x2",
        ],
        "optional_achieved_work_metrics": [],
    },
]

# SASS instruction width in bytes. 128-bit (16-byte) encoding is constant across
# every architecture Nsight Compute supports (Volta and newer), so walking PCs
# in 16-byte steps visits each instruction exactly once.
_SASS_INSTR_STRIDE = 16

# Backstop against an unterminated PC walk; far above any real kernel's length.
_SASS_MAX_INSTRS = 200000

# ncu_report MsgType enum -> short label used for colouring rule advice.
_MSG_TYPE_LABELS = {0: "none", 1: "ok", 2: "optimization", 3: "warning", 4: "error"}

# device__attribute_* metrics surfaced as a short, human-friendly device summary.
# (label, attribute suffix, formatter) — every one is also in the raw metric set.
_DEVICE_SUMMARY_FIELDS = [
    ("Device", "display_name", str),
    ("Streaming multiprocessors", "multiprocessor_count", int),
    ("Max threads / block", "max_threads_per_block", int),
    ("Max warps / SM", "max_warps_per_multiprocessor", int),
    ("Max shared mem / block", "max_shared_memory_per_block", int),
]

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


def _metric_lookup_from_kernel(kernel: dict) -> dict[str, object]:
  """Return raw metric values keyed by NCU metric name for one parsed kernel."""
  return {m["name"]: m["value"] for m in kernel["metrics"]}


def _numeric_metric(metrics: dict[str, object], name: str) -> float | None:
  """Return a finite numeric metric value, or None if NCU did not collect it."""
  value = metrics.get(name)
  if isinstance(value, (int, float)):
    numeric = float(value)
    if math.isfinite(numeric):
      return numeric
  return None


def _collect_roofline(kernels: list[dict]) -> dict:
  """Extract NCU SpeedOfLight roofline values from collected metrics.

  This follows the metric names and derived__ definitions used by
  SpeedOfLight_RooflineChart.section. The chart point is:
    achieved FLOP/s = NCU achieved work per elapsed SM cycle * SMSP cycles/s
    arithmetic intensity = achieved FLOP/s / achieved DRAM byte/s
  and the ceilings use the matching NCU peak-work and peak-traffic metrics.
  """
  rooflines: list[dict] = []
  for definition in _ROOFLINE_DEFINITIONS:
    points: list[dict] = []
    required_metric_names = [
        definition["peak_work_metric"],
        _ROOFLINE_PEAK_SM_CLOCK,
        _ROOFLINE_ACHIEVED_SM_CLOCK,
        _ROOFLINE_PEAK_TRAFFIC_PER_CYCLE,
        _ROOFLINE_DRAM_CLOCK,
        _ROOFLINE_ACHIEVED_TRAFFIC,
        *definition["required_achieved_work_metrics"],
    ]

    for kernel in kernels:
      metrics = _metric_lookup_from_kernel(kernel)
      required = {name: _numeric_metric(metrics, name) for name in required_metric_names}
      if any(value is None for value in required.values()):
        continue

      peak_compute_flop_per_cycle = required[definition["peak_work_metric"]]
      peak_sm_clock_hz = required[_ROOFLINE_PEAK_SM_CLOCK]
      achieved_sm_clock_hz = required[_ROOFLINE_ACHIEVED_SM_CLOCK]
      peak_memory_byte_per_cycle = required[_ROOFLINE_PEAK_TRAFFIC_PER_CYCLE]
      dram_clock_hz = required[_ROOFLINE_DRAM_CLOCK]
      achieved_traffic_bytes_s = required[_ROOFLINE_ACHIEVED_TRAFFIC]
      if peak_compute_flop_per_cycle <= 0 or peak_sm_clock_hz <= 0 or peak_memory_byte_per_cycle <= 0 or dram_clock_hz <= 0:
        continue
      if achieved_sm_clock_hz <= 0 or achieved_traffic_bytes_s <= 0:
        continue

      achieved_work_flop_per_cycle = 0.0
      for name in definition["required_achieved_work_metrics"]:
        achieved_work_flop_per_cycle += required[name]
      for name in definition["optional_achieved_work_metrics"]:
        value = _numeric_metric(metrics, name)
        if value is not None:
          achieved_work_flop_per_cycle += value

      achieved_flops = achieved_work_flop_per_cycle * achieved_sm_clock_hz
      compute_ceiling_flops = peak_compute_flop_per_cycle * peak_sm_clock_hz
      memory_ceiling_bytes_s = peak_memory_byte_per_cycle * dram_clock_hz
      points.append(
          {
              "kernel_idx": kernel["idx"],
              "kernel_name": kernel["name"],
              "achieved_flops": _clean_number(achieved_flops),
              "arithmetic_intensity": _clean_number(achieved_flops / achieved_traffic_bytes_s),
              "achieved_traffic_bytes_s": _clean_number(achieved_traffic_bytes_s),
              "compute_ceiling_flops": _clean_number(compute_ceiling_flops),
              "memory_ceiling_bytes_s": _clean_number(memory_ceiling_bytes_s),
              "achieved_work_flop_per_cycle": _clean_number(achieved_work_flop_per_cycle),
              "peak_compute_flop_per_cycle": _clean_number(peak_compute_flop_per_cycle),
          })

    if points:
      rooflines.append(
          {
              "id": definition["id"],
              "label": definition["label"],
              "precision": definition["precision"],
              "compute_ceiling_flops": max(p["compute_ceiling_flops"] for p in points),
              "memory_ceiling_bytes_s": max(p["memory_ceiling_bytes_s"] for p in points),
              "source_metrics": required_metric_names + definition["optional_achieved_work_metrics"],
              "points": points,
          })

  return {
      "available": bool(rooflines),
      "message": None if rooflines else _ROOFLINE_PLACEHOLDER,
      "source": "NCU SpeedOfLight_RooflineChart metrics",
      "rooflines": rooflines,
  }


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


def _function_base_pcs(action) -> list[int]:
  """Return the entry program counters of every function in the action."""
  metric = action.metric_by_name(_FUNCTION_PCS_METRIC)
  if metric is None:
    return []
  try:
    return [metric.as_uint64(i) for i in range(metric.num_instances())]
  except Exception:
    return []


def _object_field(obj, name: str):
  """Read a SWIG attribute object or dict field from ncu_report."""
  if isinstance(obj, dict):
    return obj[name]
  return getattr(obj, name)


def _collect_source(action) -> dict:
  """Reconstruct SASS/PTX listings and gather CUDA-C source for one action.

  SASS works through the ncu_report module with no ncu binary: instruction text
  is keyed by program counter. For each function entry PC we walk forward one
  instruction at a time until the first PC with no SASS, which marks the end of
  that function's contiguous code. PTX (when collected) correlates to the same
  SASS PCs; consecutive duplicate PTX lines are collapsed. CUDA-C source is read
  from source_files(). Any content type that was not collected yields an empty
  list, which the page renders as "not available".
  """
  sass: list[dict] = []
  ptx: list[dict] = []
  last_ptx: str | None = None
  for base in _function_base_pcs(action):
    i = 0
    while i < _SASS_MAX_INSTRS:
      pc = base + i * _SASS_INSTR_STRIDE
      text = action.sass_by_pc(pc)
      if not text:
        break
      sass.append({"addr": hex(pc), "text": text})
      ptx_text = action.ptx_by_pc(pc)
      if ptx_text and ptx_text != last_ptx:
        ptx.append({"addr": hex(pc), "text": ptx_text})
        last_ptx = ptx_text
      i += 1

  cuda_sources: list[dict] = []
  for fname, content in dict(action.source_files()).items():
    if content and content.strip():
      cuda_sources.append({"file": fname, "content": content})

  return {"sass": sass, "ptx": ptx, "cuda_sources": cuda_sources}


def _extract_rules(action) -> list[dict]:
  """Normalize rule recommendations into JSON-safe dicts.

  Returns [] for reports without rules (e.g. a narrow --metrics capture). Each
  entry carries the advice message plus its severity label so the page can
  colour it green/yellow. Rule objects come from a SWIG layer whose exact return
  types we cannot exercise here, so a malformed rule is logged and skipped rather
  than allowed to fault the whole page.
  """
  rules: list[dict] = []
  for rule_result in action.rule_results():
    try:
      entry: dict = {
          "name": rule_result.name(),
          "section": rule_result.section_identifier(),
      }
      if rule_result.has_rule_message():
        msg = rule_result.rule_message()
        entry["title"] = str(_object_field(msg, "title"))
        entry["message"] = str(_object_field(msg, "message"))
        msg_type = int(_object_field(msg, "type"))
        entry["type"] = msg_type
        entry["type_label"] = _MSG_TYPE_LABELS.get(msg_type, "none")
      if rule_result.has_speedup_estimation():
        entry["speedup_pct"] = _clean_number(float(_object_field(rule_result.speedup_estimation(), "speedup")))
      rules.append(entry)
    except Exception as exc:
      log.warning("ncu_rule_normalize_failed", error=str(exc))
  return rules


def _device_attributes(action) -> list[dict]:
  """Collect device__attribute_* metrics for the Session / Device tab.

  These are part of the raw metric set, so the tab is a rendered view of them.
  """
  attrs: list[dict] = []
  for name in action.metric_names():
    if not name.startswith("device__attribute_"):
      continue
    value, unit = _metric_value(action, name)
    if value is None:
      continue
    attrs.append({"name": name, "value": value, "unit": unit or ""})
  attrs.sort(key=lambda a: a["name"])
  return attrs


def _device_summary(action) -> list[dict]:
  """A short, human-friendly device summary derived from device attributes."""
  summary: list[dict] = []
  major = _metric_value(action, "device__attribute_compute_capability_major")[0]
  minor = _metric_value(action, "device__attribute_compute_capability_minor")[0]
  for label, suffix, fmt in _DEVICE_SUMMARY_FIELDS:
    value = _metric_value(action, f"device__attribute_{suffix}")[0]
    if value is None:
      continue
    summary.append({"label": label, "value": fmt(value)})
    if suffix == "display_name" and major is not None and minor is not None:
      summary.append({"label": "Compute capability", "value": f"{int(major)}.{int(minor)}"})
  return summary


def _action_to_dict(action, idx: int) -> dict:
  """Build the per-kernel record: summary fields, raw metrics, rules, source."""
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
      "rules": _extract_rules(action),
      "sections": None,
      **_collect_source(action),
  }


def _parse_with_module(module, abspath: str) -> dict:
  """Parse a report using the ncu_report module."""
  try:
    ctx = module.load_report(abspath)
  except Exception as exc:
    raise NcuParseError(f"Unable to load report: {exc}") from exc

  kernels: list[dict] = []
  device: str | None = None
  first_action = None
  for i in range(ctx.num_ranges()):
    rng = ctx.range_by_idx(i)
    for j in range(rng.num_actions()):
      action = rng.action_by_idx(j)
      if first_action is None:
        first_action = action
      kernels.append(_action_to_dict(action, len(kernels)))
      if device is None:
        device_value = _metric_value(action, _DEVICE_METRIC)[0]
        if isinstance(device_value, str):
          device = device_value

  if not kernels:
    raise NcuParseError("Report contains no profiled kernels.")

  sections = _sections_from_csv(abspath)
  if sections is not None:
    for kernel in kernels:
      kernel["sections"] = sections.get(kernel["idx"])

  return {
      "kernels": kernels,
      "device": device,
      "parser": "ncu_report",
      "device_attributes": _device_attributes(first_action),
      "device_summary": _device_summary(first_action),
      "roofline": _collect_roofline(kernels),
  }


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


def _group_rows_into_sections(rows: list[dict]) -> dict[int, list[dict]]:
  """Group `--csv --page details` rows into per-action ordered sections.

  Returns {action_index: [{"name", "metrics": [{name,value,unit}]}]} keyed by the
  position of each action ID in first-seen order, matching the kernel ordering of
  both parsers. Section and metric order follow the ncu output.
  """
  order: list[str] = []
  by_id: dict[str, list[dict]] = {}
  section_lookup: dict[str, dict[str, dict]] = {}
  for row in rows:
    action_id = row.get("ID")
    if action_id is None:
      continue
    if action_id not in by_id:
      order.append(action_id)
      by_id[action_id] = []
      section_lookup[action_id] = {}
    section_name = (row.get("Section Name") or "").strip() or "Other metrics"
    section = section_lookup[action_id].get(section_name)
    if section is None:
      section = {"name": section_name, "metrics": []}
      section_lookup[action_id][section_name] = section
      by_id[action_id].append(section)
    metric_name = row.get("Metric Name")
    if metric_name:
      section["metrics"].append(
          {
              "name": metric_name,
              "value": _coerce_csv_value(row.get("Metric Value", "")),
              "unit": (row.get("Metric Unit") or "").strip(),
          })
  return {idx: by_id[action_id] for idx, action_id in enumerate(order)}


def _sections_from_csv(abspath: str) -> dict[int, list[dict]] | None:
  """Best-effort section grouping via `ncu --import ... --csv --page details`.

  The ncu_report module does not expose a metric's section membership, so we read
  it from the details page. Returns None when the ncu binary is unavailable or
  the import fails, in which case the Details tab degrades to a note pointing at
  the Raw metrics tab rather than erroring.
  """
  cmd = ["ncu", "--import", abspath, "--csv", "--page", "details"]
  try:
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=SUBPROCESS_NCU_CSV_IMPORT_TIMEOUT)
  except (FileNotFoundError, subprocess.TimeoutExpired):
    log.warning("ncu_sections_csv_unavailable")
    return None
  if proc.returncode != 0:
    log.warning("ncu_sections_csv_failed", detail=(proc.stderr or proc.stdout).strip()[:200])
    return None
  return _group_rows_into_sections(list(csv.DictReader(io.StringIO(proc.stdout))))


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

  rows = list(csv.DictReader(io.StringIO(proc.stdout)))
  by_id: dict[str, dict] = {}
  order: list[str] = []
  for row in rows:
    action_id = row.get("ID")
    if action_id is None:
      continue
    if action_id not in by_id:
      order.append(action_id)
      by_id[action_id] = {
          "name": row.get("Kernel Name", ""),
          "grid": _format_dims_csv(row.get("Grid Size", "")),
          "block": _format_dims_csv(row.get("Block Size", "")),
          "cc": (row.get("CC") or "").strip(),
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

  sections = _group_rows_into_sections(rows)
  kernels: list[dict] = []
  for idx, action_id in enumerate(order):
    entry = by_id[action_id]
    raw_metrics = entry["metrics"]
    dur_value, dur_unit = raw_metrics.get(_DURATION_METRIC, (None, None))
    metrics = [{"name": n, "value": v, "unit": u} for n, (v, u) in raw_metrics.items() if v is not None]
    # The CSV fallback cannot extract SASS/PTX/source or rules; those tabs render
    # an "unavailable via the CSV fallback parser" note.
    kernels.append(
        {
            "idx": idx,
            "name": entry["name"],
            "duration_ns": _duration_ns(dur_value, dur_unit),
            "grid": entry["grid"],
            "block": entry["block"],
            "occupancy_pct": raw_metrics.get(_OCCUPANCY_METRIC, (None, None))[0],
            "sol_compute_pct": raw_metrics.get(_SOL_COMPUTE_METRIC, (None, None))[0],
            "sol_memory_pct": raw_metrics.get(_SOL_MEMORY_METRIC, (None, None))[0],
            "metrics": metrics,
            "rules": [],
            "sections": sections.get(idx),
            "sass": [],
            "ptx": [],
            "cuda_sources": [],
        })

  device_summary: list[dict] = []
  first_cc = by_id[order[0]]["cc"]
  if first_cc:
    device_summary.append({"label": "Compute capability", "value": first_cc})

  return {
      "kernels": kernels,
      "device": None,
      "parser": "csv",
      "device_attributes": [],
      "device_summary": device_summary,
      "roofline": _collect_roofline(kernels),
  }


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
