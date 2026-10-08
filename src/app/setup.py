"""The setup run behind ``scripts/setup.sh``: ``python -m src.app.setup [--dry-run]``.

The run registers the packages, provisions the ``~/.charliebot`` home layout, then calls each setup step
that a package registered with ``wiring.register_setup_step``, in registration order. With ``--dry-run``
nothing is written: the home block reports what exists and what it would create, and each step receives
``dry_run=True``.

``python -m src.app.setup --step <module>:<attr>`` registers the packages and calls that one function with
``dry_run=False``. It runs no home block and no registered step, and the function need not be a registered step.
A step that needs a fresh interpreter (after ``uv sync``) re-enters through this form.
"""

import argparse
import asyncio
import importlib
from collections.abc import Sequence

from src.app import registrations
from src.infra.config import get_config
from src.runtime import init_seed
from src.runtime.hooks import wiring


def _provision_home(cfg, *, dry: bool) -> None:
  print("==> Provisioning ~/.charliebot")
  # Per-item created/exists for the home layout. init_charliebot_home() is the
  # single source of truth for this layout and is the same path the server runs at startup;
  # in dry-run we only report what already exists vs what would be created, and write nothing.
  # The memory store creates its own scaffold at first use, so setup leaves ~/.charliebot/memory/ alone.
  home_items = [
      ("dir", "~/.charliebot/", cfg.charliebot_home),
      ("dir", "~/.charliebot/sessions/", cfg.sessions_dir),
      ("dir", "~/.charliebot/config.d/", cfg.config_d_dir),
      ("file", "~/.charliebot/config.yaml", cfg.config_file),
      ("file", "~/.charliebot/credentials.yaml", cfg.credentials_file),
  ]
  existed_before = {str(p): p.exists() for _, _, p in home_items}
  if not dry:
    asyncio.run(init_seed.init_charliebot_home())
  for label, path in [(lbl, p) for _, lbl, p in home_items]:
    now_exists = path.exists()
    if dry:
      status = "exists" if now_exists else "would-create"
    else:
      status = "exists" if existed_before[str(path)] else "created"
    print(f"  home {label}: {status}")


def main(argv: Sequence[str] | None = None) -> None:
  parser = argparse.ArgumentParser(prog="python -m src.app.setup", description=__doc__.splitlines()[0])
  mode = parser.add_mutually_exclusive_group()
  mode.add_argument("--dry-run", action="store_true", help="write nothing; print what each step would do")
  mode.add_argument("--step", metavar="MODULE:ATTR", help="call one function with dry_run=False and nothing else")
  args = parser.parse_args(argv)

  registrations.register_all()
  cfg = get_config()

  if args.step is not None:
    module, separator, attr = args.step.partition(":")
    if not (module and separator and attr):
      parser.error(f"--step takes MODULE:ATTR, got {args.step!r}")
    getattr(importlib.import_module(module), attr)(cfg, dry_run=False)
    return

  _provision_home(cfg, dry=args.dry_run)
  print("  Reminder: fill in the secret key charliebot_access_key before first start.")
  for step in wiring.setup_steps():
    step(cfg, dry_run=args.dry_run)


if __name__ == "__main__":
  main()
