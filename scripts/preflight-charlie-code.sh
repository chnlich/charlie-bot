#!/usr/bin/env bash
# Preflight for a `type: charlie-code` backend option: asserts every mechanism a
# session on it depends on — the config entry exists, the charlie-code binary
# resolves and runs, the CLI contract matches what CharlieCodeBackend emits, and
# the endpoint serves the configured model — so one run covers the whole fault
# class before a session or delegation is started on it.
set -euo pipefail

# Resolve paths relative to this script so preflight works from any current directory.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

usage() {
  echo "Usage: $0 <backend-option-id>"
}

if [[ $# -eq 0 ]]; then
  usage >&2
  exit 2
fi

if [[ "$1" == "-h" || "$1" == "--help" ]]; then
  usage
  exit 2
fi

if [[ $# -ne 1 ]]; then
  usage >&2
  exit 2
fi

cd "$REPO_ROOT"

# The failure lines below are the script's stderr contract, so uv must not print
# environment warnings ahead of them: an inherited VIRTUAL_ENV (e.g. another
# checkout's venv) makes uv warn about the mismatch it ignores anyway.
unset VIRTUAL_ENV

uv run python - "$1" <<'PY'
import json
import os
import subprocess
import sys
import urllib.request


def fail(message: str):
  print(message, file=sys.stderr)
  raise SystemExit(1)


backend_id = sys.argv[1]

# Mechanism 1: the config entry exists and is a charlie-code backend.
from src.core.config import get_config

cfg = get_config()
option = cfg.get_backend_option(backend_id)
if option is None:
  fail(
      "option-missing: add a backend_options entry with id "
      f"{backend_id} and type charlie-code to ~/.charliebot/config.yaml"
  )
if option.type != "charlie-code":
  fail(
      f"option-missing: backend option {backend_id} has type {option.type}, not charlie-code; "
      f"add a backend_options entry with id {backend_id} and type charlie-code to ~/.charliebot/config.yaml"
  )

# Mechanism 2: the backend builds, so its binary resolves.
from src.agents.backends.registry import build_backend

try:
  backend = build_backend(option, cfg)
except FileNotFoundError:
  fail(
      "binary-missing: install charlie-code so it resolves on PATH or in ~/.local/bin "
      "(uv tool install -e ~/workspace/charlie-code)"
  )
except ValueError as e:
  fail(f"option-missing: {e}")

# Mechanism 3: the binary actually runs.
try:
  help_run = subprocess.run(
      [backend._bin, "--help"],
      env=backend._prepare_env(dict(os.environ)),
      capture_output=True,
      text=True,
      timeout=60,
  )
except Exception as e:
  fail(f"binary-broken: {e}; reinstall charlie-code (uv tool install -e ~/workspace/charlie-code)")
if help_run.returncode != 0:
  first_stderr = next((line for line in help_run.stderr.splitlines() if line.strip()), "")
  fail(
      f"binary-broken: {first_stderr}; reinstall charlie-code "
      "(uv tool install -e ~/workspace/charlie-code)"
  )

# Mechanism 4: the CLI contract matches what CharlieCodeBackend emits.
for flag in ["--task-file", "--json", "--model", "--api-base", "--resume", "--context-window"]:
  if flag not in help_run.stdout:
    fail(f"contract-missing: charlie-code --help lacks {flag}; update the charlie-code checkout and reinstall")

# Mechanism 5: the endpoint serves the configured model.
api_base = option.api_base.rstrip("/")
url = f"{api_base}/models"
try:
  with urllib.request.urlopen(url, timeout=15) as resp:
    payload = json.load(resp)
  served_ids = sorted(str(m["id"]) for m in payload["data"])
except Exception as e:
  fail(f"model-not-served: {url} unreachable ({e})")
# The litellm provider prefix is stripped at request time; the endpoint serves the bare id.
served_model = option.model.split("/", 1)[1] if "/" in option.model else option.model
if served_model not in served_ids:
  fail(f"model-not-served: {url} lists {served_ids}; fix the model field or the endpoint")

print(f"OK: {backend_id} -> {backend._bin} serves {served_model} at {api_base}")
PY
