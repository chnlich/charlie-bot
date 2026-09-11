"""The CLI's import-weight contract: `src.cli.common` stays off the server's heavy chains.

Every master turn and worker session runs several `charliebot` invocations, each a
fresh process, so `src.cli.common` — the module every command imports — must not drag
the backend stack (`src.agents.backends.base`), the sessions stack (`src.core.threads`,
`src.core.sessions`), or numpy (`src.core.runs`) into processes that only parse args,
read config, and POST to the internal API. The constants they need live in
`src.core.models`, which config already pays for.
"""

import json
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

HEAVY_MODULES = (
    "src.agents.backends.base",
    "src.core.threads",
    "src.core.sessions",
    "src.core.runs",
    "numpy",
)


def _modules_loaded_after_common_import() -> list[str]:
  code = (
      "import json, sys; import src.cli.common; "
      f"print(json.dumps(sorted(set(sys.modules) & {set(HEAVY_MODULES)!r})))")
  proc = subprocess.run(
      [sys.executable, "-c", code],
      cwd=REPO_ROOT,
      capture_output=True,
      text=True,
      timeout=120,
      check=True,
  )
  return json.loads(proc.stdout)


def test_cli_common_imports_without_the_heavy_chains() -> None:
  loaded = _modules_loaded_after_common_import()
  assert loaded == [], (
      "src.cli.common pulled the server's heavy chains into the CLI process: "
      f"{loaded}; the CLI startup budget (docs/perf_baseline.md M92) depends on "
      "these staying out — import them lazily at the use site that needs them")
