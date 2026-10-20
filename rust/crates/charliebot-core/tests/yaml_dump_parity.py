"""Dump cron-file values the way PyYAML's safe C dumper does, and check that each Rust dump reads back as its value.

stdin: a JSON list of {"value": <JSON value>, "rust": <the Rust writer's dump of that value>}.
stdout: a JSON list of {"pyyaml": <PyYAML's dump of the value>, "reads_back": <whether yaml.safe_load of the Rust dump
gives the value back>}.
"""

import json
import sys

import yaml


def dump(value: object) -> str:
  return yaml.dump(value, Dumper=yaml.CSafeDumper, allow_unicode=True, default_flow_style=False, sort_keys=False)


def check(case: dict) -> dict:
  return {"pyyaml": dump(case["value"]), "reads_back": yaml.safe_load(case["rust"]) == case["value"]}


json.dump([check(case) for case in json.load(sys.stdin)], sys.stdout)
