"""The CLI's import-weight contract: `src.cli.common` stays off the server's heavy chains.

Every master turn and worker session runs several `charliebot` invocations, each a
fresh process, so `src.cli.common` — the module every command imports — must not drag
the backend stack (`src.agents.backends.base`), the sessions stack (`src.core.threads`,
`src.core.sessions`), numpy (`src.core.runs`), or the logging stack (`structlog`, whose
import eagerly pulls structlog.dev — rich, pygments) into processes that only parse
args, read config, and POST to the internal API. The constants they need live in
`src.core.models`, which config already pays for; config's own logger imports
structlog on first use instead.
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
    "structlog",
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


def test_config_defers_structlog_until_the_first_log_call() -> None:
  # The probe's warning line and the result JSON both reach the subprocess's
  # streams; the JSON rides stderr so the parse sees it alone.
  code = (
      "import json, sys; import src.core.config; "
      "before = 'structlog' in sys.modules; src.core.config.log.warning('m92_probe'); "
      "sys.stderr.write(json.dumps([before, 'structlog' in sys.modules]))")
  proc = subprocess.run(
      [sys.executable, "-c", code],
      cwd=REPO_ROOT,
      capture_output=True,
      text=True,
      timeout=120,
      check=True,
  )
  before, after = json.loads(proc.stderr)
  assert before is False, (
      "config imported structlog at module import; every CLI invocation pays "
      "structlog.dev (rich, pygments) for log lines config never emits")
  assert after is True, "config.log did not resolve structlog on first use"
