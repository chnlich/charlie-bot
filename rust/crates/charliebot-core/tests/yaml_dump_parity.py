"""Emit PyYAML's C safe dumper output for a JSON shaped cron-file value."""

import json
import sys

import yaml


value = json.load(sys.stdin)
sys.stdout.write(yaml.dump(value, Dumper=yaml.CSafeDumper, allow_unicode=True, default_flow_style=False, sort_keys=False))
