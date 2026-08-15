"""CLI: read a CharlieBot config key wherever it lives.

A top-level key may sit in ``config.yaml`` or in any ``config.d/<topic>.yaml``
fragment, so reading a key by opening one file by path is wrong. ``get``
resolves through :func:`get_config`, which sees the merged view of both:

  charliebot config get <key>

Stdout carries exactly the value (scalars bare, lists/mappings/models as JSON)
so callers can substitute ``$(charliebot config get some_key)`` into commands;
all diagnostics go to stderr.
"""

import argparse
import json
import sys

from pydantic import BaseModel

from src.core.config import CharlieBotConfig, get_config


def main() -> None:
  parser = argparse.ArgumentParser(description="Read CharlieBot config keys regardless of which file holds them")
  sub = parser.add_subparsers(dest="command", required=True)

  p_get = sub.add_parser("get", help="Print a config key's value to stdout (nothing else)")
  p_get.add_argument("key", help="Top-level CharlieBotConfig field name")

  args = parser.parse_args()
  if args.command == "get":
    _cmd_get(args.key)


def _cmd_get(key: str) -> None:
  if key not in CharlieBotConfig.model_fields:
    print(f"error: unknown config key: {key} (not a CharlieBotConfig field)", file=sys.stderr)
    sys.exit(2)
  value = getattr(get_config(), key)
  # An unset (None) credential must fail rather than print "None" into a
  # caller's variable.
  if value is None:
    print(f"error: config key {key} is unset", file=sys.stderr)
    sys.exit(1)
  if isinstance(value, (BaseModel, list, dict)):
    print(json.dumps(value, default=lambda o: o.model_dump()))
  else:
    print(value)


if __name__ == "__main__":
  main()
