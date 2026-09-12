"""The CLI's import-weight contract: `src.cli.common` stays off the server's heavy chains.

Every master turn and worker session runs several `charliebot` invocations, each a
fresh process, so `src.cli.common` — the module every command imports — must not drag
the backend stack (`src.agents.backends.base`), the sessions stack (`src.core.threads`,
`src.core.sessions`), numpy (`src.core.runs`), the logging stack (`structlog`, whose
import eagerly pulls structlog.dev — rich, pygments), the HTTP client (`requests`,
urllib3 + charset_normalizer, ~100 ms of the M92 floor), or the config stack
(`src.core.config`, whose pydantic models + yaml chain is ~180 ms of the M92 floor)
into processes that only parse args, read config, and POST to the internal API.
config loads on first call through the get_config/get_credentials forwarders; the
constants the argparse layer needs single-home in `src.core.constants` (stdlib-only).
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
    "src.core.config",
    "src.core.models",
    "numpy",
    "structlog",
    "requests",
    "pydantic",
)

# The plan chain's extra bans: the validation gate's registry stack and the web
# framework, none of which a plan command touches before its request.
PLAN_HEAVY_MODULES = HEAVY_MODULES + (
    "fastapi",
    "src.core.artifact_check",
    "src.agents.backends.registry",
)

# The memory chain's ban set: structlog (the log proxy defers it) and the replay-curation
# stack, which only the replay/experiment/compare verbs run (lazy imports inside the _cmd_
# functions). config + models + pydantic stay out of the ban set: the get_config
# module-attribute contract (tests/test_memory_store.py) and every verb's config read
# bind them at import.
MEMORY_HEAVY_MODULES = (
    "src.agents.backends.base",
    "src.core.threads",
    "src.core.sessions",
    "src.core.runs",
    "numpy",
    "structlog",
    "requests",
    "src.core.memory_replay",
)


def _modules_loaded_after_import(module_expr: str, heavy: tuple[str, ...]) -> list[str]:
  code = ("import json, sys; "
          f"{module_expr}; "
          f"print(json.dumps(sorted(set(sys.modules) & {set(heavy)!r})))")
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
  loaded = _modules_loaded_after_import("import src.cli.common", HEAVY_MODULES)
  assert loaded == [], (
      "src.cli.common pulled the server's heavy chains into the CLI process: "
      f"{loaded}; the CLI startup budget (docs/perf_baseline.md M92) depends on "
      "these staying out — import them lazily at the use site that needs them")


def test_plan_chain_imports_without_the_heavy_chains() -> None:
  # Every plan command imports the module and builds the parser (the amend/close
  # choices ride src.core.constants, so parser build stays light too); the heavy
  # chains load only inside the verb paths that need them (artifact check inside
  # the validation to_thread hop).
  loaded = _modules_loaded_after_import("import src.cli.plan; src.cli.plan._build_parser()", PLAN_HEAVY_MODULES)
  assert loaded == [], (
      "the plan command chain pulled the server's heavy chains into the CLI "
      f"process: {loaded}; the M97 command wall (docs/perf_baseline.md) depends "
      "on these staying out — import them lazily at the use site that needs them")


def test_plan_constants_match_the_model_literals() -> None:
  # The type home is models' Literal; the stdlib tuples the CLI parses with must
  # stay its exact runtime image.
  code = (
      "import json; from typing import get_args; "
      "import src.core.constants as c; import src.core.models as m; "
      "print(json.dumps([list(c.PLAN_AMEND_TRIGGERS), list(get_args(m.PlanAmendTrigger)), "
      "list(c.PLAN_CLOSE_MODES), list(get_args(m.PlanCloseMode))]))")
  proc = subprocess.run(
      [sys.executable, "-c", code],
      cwd=REPO_ROOT,
      capture_output=True,
      text=True,
      timeout=120,
      check=True,
  )
  amend_tuple, amend_literal, close_tuple, close_literal = json.loads(proc.stdout)
  assert amend_tuple == amend_literal and close_tuple == close_literal, (
      "src.core.constants' plan vocabularies drifted from the models Literals: "
      f"{amend_tuple} vs {amend_literal}; {close_tuple} vs {close_literal}")


def test_memory_chain_imports_without_the_heavy_chains() -> None:
  loaded = _modules_loaded_after_import("import src.cli.memory", MEMORY_HEAVY_MODULES)
  assert loaded == [], (
      "the memory command chain pulled the replay stack or structlog into the CLI "
      f"process: {loaded}; the M98 invocation wall (docs/perf_baseline.md) depends "
      "on these staying out — the replay verbs import their stack inside the verb "
      "path, and src.core.memory's log proxy defers structlog to first use")


def test_memory_defers_structlog_until_the_first_log_call() -> None:
  # The probe's warning line and the result JSON both reach the subprocess's
  # streams; the JSON rides stderr so the parse sees it alone.
  code = (
      "import json, sys; import src.core.memory; "
      "before = 'structlog' in sys.modules; src.core.memory.log.warning('m98_probe'); "
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
      "src.core.memory imported structlog at module import; every memory CLI "
      "invocation pays structlog.dev (rich, pygments) for the error-path log "
      "lines a read command never emits")
  assert after is True, "memory.log did not resolve structlog on first use"


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
