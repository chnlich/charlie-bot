"""Seed and first-run initialization of the ~/.charliebot/ directory structure."""

import os
import shutil

from src.infra import config, yaml_utils


def _default_config_yaml() -> dict:
  """Build the default config dict with placeholder values."""
  return {
      "paths": {
          "workspace_dirs": ["~/workspace"],
          "worktree_dir": "~/worktrees",
      },
  }


async def init_charliebot_home() -> None:
  """Ensure ~/.charliebot/ directory structure exists and seed default files."""
  cfg = config.get_config()

  # Create all required directories
  dirs = [
      cfg.charliebot_home,
      cfg.sessions_dir,
      cfg.config_d_dir,
  ]
  for d in dirs:
    d.mkdir(parents=True, exist_ok=True)

  # Seed config.yaml from the committed template if missing
  if not cfg.config_file.exists():
    template = cfg.charlie_bot_repo / "configs" / "config.example.yaml"
    if template.exists():
      cfg.config_file.write_text(template.read_text(encoding="utf-8"), encoding="utf-8")
    else:
      yaml_utils.save_yaml(cfg.config_file, _default_config_yaml())

  # Seed credentials.yaml from the committed template if missing. The file holds
  # this profile's secrets, so it is created owner-readable only (0600); the
  # template in the repo is all comments, so the seeded file loads as empty
  # sections until the operator fills values in.
  if not cfg.credentials_file.exists():
    template = cfg.charlie_bot_repo / "configs" / "credentials.example.yaml"
    shutil.copyfile(template, cfg.credentials_file)
    os.chmod(cfg.credentials_file, 0o600)
