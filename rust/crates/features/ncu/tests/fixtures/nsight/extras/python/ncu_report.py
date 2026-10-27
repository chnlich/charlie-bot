"""A synthetic stand-in for NVIDIA's ncu_report module, for the Rust ncu tests.

A report is the JSON the crate's tests write with `write_report` and `module_report` (src/test_support.rs). The shape
of each object follows the calls that src/features/ncu/ncu_parsing.py makes on the real module, and the values are the
ones the report holds. A report with a "refuse" message fails to load, as a report the real module rejects does.
"""

import json
import pathlib


class _Metric:
  def __init__(self, value, unit):
    self._value = value
    self._unit = unit

  def has_value(self):
    return self._value is not None

  def value(self):
    return self._value

  def unit(self):
    return self._unit


class _FunctionPcs:
  def __init__(self, pcs):
    self._pcs = pcs

  def has_value(self):
    return True

  def value(self):
    return self._pcs[0]

  def unit(self):
    return ""

  def num_instances(self):
    return len(self._pcs)

  def as_uint64(self, index):
    return self._pcs[index]


class _RuleResult:
  def __init__(self, rule):
    self._rule = rule

  def name(self):
    return self._rule["name"]

  def section_identifier(self):
    return self._rule["section"]

  def has_rule_message(self):
    return "message" in self._rule

  def rule_message(self):
    return {"title": self._rule["title"], "message": self._rule["message"], "type": self._rule["type"]}

  def has_speedup_estimation(self):
    return "speedup_pct" in self._rule

  def speedup_estimation(self):
    return {"speedup": self._rule["speedup_pct"]}


class _Action:
  def __init__(self, data):
    self._data = data
    self._metrics = {metric["name"]: _Metric(metric["value"], metric["unit"]) for metric in data["metrics"]}
    if data.get("function_pcs"):
      self._metrics["launch__function_pcs"] = _FunctionPcs(data["function_pcs"])
    self._sass = {entry["pc"]: entry["text"] for entry in data["sass"]}
    self._ptx = {entry["pc"]: entry["text"] for entry in data["ptx"]}

  def name(self):
    return self._data["name"]

  def metric_names(self):
    return list(self._metrics)

  def metric_by_name(self, name):
    return self._metrics.get(name)

  def rule_results(self):
    return [_RuleResult(rule) for rule in self._data["rules"]]

  def sass_by_pc(self, pc):
    return self._sass.get(pc, "")

  def ptx_by_pc(self, pc):
    return self._ptx.get(pc, "")

  def source_files(self):
    return list(self._data["source_files"].items())


class _Range:
  def __init__(self, actions):
    self._actions = actions

  def num_actions(self):
    return len(self._actions)

  def action_by_idx(self, index):
    return self._actions[index]


class _Context:
  def __init__(self, data):
    self._range = _Range([_Action(kernel) for kernel in data["kernels"]])

  def num_ranges(self):
    return 1

  def range_by_idx(self, index):
    return self._range


def load_report(path):
  data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
  if data.get("refuse"):
    raise RuntimeError(data["refuse"])
  return _Context(data)
