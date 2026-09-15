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

import pytest

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

# The plan chain's extra bans: the validation gate's registry stack, the web
# framework, and plan_diff — its difflib + html subtree (~10 ms net of the pydantic
# shared chain) serves only the diff verb's text render, and no sync plan command
# touches it before its request.
PLAN_HEAVY_MODULES = HEAVY_MODULES + (
    "fastapi",
    "src.core.artifact_check",
    "src.agents.backends.registry",
    "src.core.plan_diff",
)

# The memory chain's ban set: structlog (the log proxy defers it to first use).
# config + models + pydantic stay out of the ban set: the get_config
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
)


def _run_probe(code: str) -> subprocess.CompletedProcess[str]:
  # Every probe forks one fresh interpreter against the repo root: the imports
  # must resolve to the checked-out sources, and a probe crash fails the test
  # (check=True) instead of parsing an empty stream.
  return subprocess.run(
      [sys.executable, "-c", code],
      cwd=REPO_ROOT,
      capture_output=True,
      text=True,
      timeout=120,
      check=True,
  )


def _modules_loaded_after_import(module_expr: str, heavy: tuple[str, ...]) -> list[str]:
  code = ("import json, sys; "
          f"{module_expr}; "
          f"print(json.dumps(sorted(set(sys.modules) & {set(heavy)!r})))")
  return json.loads(_run_probe(code).stdout)


def test_cli_common_defers_buildinfo() -> None:
  # buildinfo drags subprocess (~4 ms of the M92 floor) and serves only the
  # version-skew failure path; the parser-build path never reads a SHA.
  loaded = _modules_loaded_after_import("import src.cli.common", ("src.core.buildinfo",))
  assert loaded == [], (
      "src.cli.common pulled buildinfo into the CLI process: "
      f"{loaded}; the M92 floor (docs/perf_baseline.md) depends on this "
      "staying out — import it lazily at the use site that needs it")


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
  proc = _run_probe(code)
  amend_tuple, amend_literal, close_tuple, close_literal = json.loads(proc.stdout)
  assert amend_tuple == amend_literal and close_tuple == close_literal, (
      "src.core.constants' plan vocabularies drifted from the models Literals: "
      f"{amend_tuple} vs {amend_literal}; {close_tuple} vs {close_literal}")


# The artifact chain's ban set: the probe's registry stack (backends.registry →
# fastapi + sessions, autonamer → sessions + streaming), asyncio (~35 ms —
# pydantic_core is absent from this chain, so asyncio's import is unshared),
# the headless renderer (its websockets stack ~60 ms), and the KaTeX fetch's
# HTTP client serve only the check/wrap verb bodies — the probe imports its
# stack inside run_probe, the page-height assertion imports the renderer inside
# _measure_page_height, and the vendored-KaTeX steady state never fetches.
ARTIFACT_HEAVY_MODULES = HEAVY_MODULES + (
    "fastapi",
    "asyncio",
    "websockets",
    "src.core.headless_render",
    "src.agents.backends.registry",
    "src.agents.backends.base",
    "src.core.autonamer",
    "src.core.sessions",
    "src.core.streaming",
)


def test_artifact_chain_imports_without_the_heavy_chains() -> None:
  loaded = _modules_loaded_after_import(
      "import src.cli.artifact; src.cli.artifact._build_parser()", ARTIFACT_HEAVY_MODULES)
  assert loaded == [], (
      "the artifact command chain pulled the probe's registry stack or the HTTP "
      f"client into the CLI process: {loaded}; the M102 command wall "
      "(docs/perf_baseline.md) depends on these staying out — run_probe imports "
      "the registry stack inside the probe, and ensure_vendored_katex imports "
      "requests on the CDN-fetch path only")


def test_memory_chain_imports_without_the_heavy_chains() -> None:
  loaded = _modules_loaded_after_import("import src.cli.memory", MEMORY_HEAVY_MODULES)
  assert loaded == [], (
      "the memory command chain pulled a heavy chain or structlog into the CLI "
      f"process: {loaded}; the M98 invocation wall (docs/perf_baseline.md) depends "
      "on these staying out — src.core.memory's log proxy defers structlog to first use")


# Modules whose log proxy defers structlog to first use. Each imports on an
# error path only, so an eager structlog import would tax every invocation for
# lines the read path never emits; the probe pin: `import` alone must leave
# structlog unloaded, and the first `log.warning` must load it.
_STRUCTLOG_DEFERRAL_CASES = [
    pytest.param(
        "src.core.config",
        "every CLI invocation pays structlog.dev (rich, pygments) for log lines config never emits",
        id="config",
    ),
    pytest.param(
        "src.core.memory",
        "every memory CLI invocation pays structlog.dev (rich, pygments) "
        "for the error-path log lines a read command never emits",
        id="memory",
    ),
]


@pytest.mark.parametrize(("module_name", "import_cost"), _STRUCTLOG_DEFERRAL_CASES)
def test_module_defers_structlog_until_the_first_log_call(module_name: str, import_cost: str) -> None:
  # The probe's warning line and the result JSON both reach the subprocess's
  # streams; the JSON rides stderr so the parse sees it alone.
  code = (
      "import json, sys; "
      f"import {module_name}; "
      "before = 'structlog' in sys.modules; "
      f"{module_name}.log.warning('probe'); "
      "sys.stderr.write(json.dumps([before, 'structlog' in sys.modules]))")
  proc = _run_probe(code)
  before, after = json.loads(proc.stderr)
  assert before is False, f"{module_name} imported structlog at module import; {import_cost}"
  assert after is True, f"{module_name}.log did not resolve structlog on first use"


# The server import floor's ban set (docs/perf_baseline.md M99): numpy rides
# src.agents.transcriber (voice) and the two SIMD scanners (ndjson's count,
# sessions' parent-reference frames), all of which load lazily at their use
# sites; structlog rides the log proxy (~77 ms of the floor, lines the import
# path never emits); httpx (~60 ms with rich) rides src.core.http and the
# backends' outbound clients, which load it on first use; croniter rides its
# two next-run resolutions (the scheduler tick, the /scheduled handler, ~21 ms
# with dateutil) and websockets rides the Slack listener's connect loop (~13 ms);
# the backends stack rides its two spawn-path builds (the autonamer naming round
# and the recap summarize, ~65 ms through src.agents.backends.registry and the
# opencode/charlie_code module bodies), which load it on first use via the shared
# load_build_backend (src/agents/backends/deferred_build.py).
SERVER_HEAVY_MODULES = (
    "numpy",
    "src.agents.transcriber",
    "structlog",
    "httpx",
    "croniter",
    "dateutil",
    "websockets",
    "src.agents.backends.registry",
    "src.agents.backends.opencode",
    "src.agents.backends.charlie_code",
)


def test_server_import_defers_the_speech_stack() -> None:
  loaded = _modules_loaded_after_import("import server", SERVER_HEAVY_MODULES)
  assert loaded == [], (
      "import server pulled the speech stack (numpy, src.agents.transcriber) at "
      f"module import: {loaded}; the M99 server import floor "
      "(docs/perf_baseline.md) depends on it loading on the provisioning thread "
      "and at the voice use sites — import it lazily there")
  code = (
      "import sys; import server; "
      "from src.api import voice; "
      "print('numpy' in sys.modules or 'src.agents.transcriber' in sys.modules)")
  pulled = _run_probe(code).stdout.strip()
  assert pulled == "False", "importing the voice route pulled the speech stack at module import"


# The sessions chain's extra bans: session_usage's usage math reads the opencode
# compaction reserve from src.core.constants (the #1412 stdlib-only home), so the
# chain imports no backend module for it.
SESSIONS_HEAVY_MODULES = (
    "src.agents.backends.registry", "src.agents.backends.opencode", "src.agents.backends.charlie_code")


def test_sessions_chain_imports_without_the_backends_stack() -> None:
  loaded = _modules_loaded_after_import("import src.core.sessions", SESSIONS_HEAVY_MODULES)
  assert loaded == [], (
      "the sessions chain pulled the backends stack into the server process: "
      f"{loaded}; the M99 server import floor (docs/perf_baseline.md) depends on "
      "src.core.session_usage reading the compaction reserve from src.core.constants "
      "— import backends lazily at the use site that builds one")


def test_autonamer_and_recap_defer_the_registry_until_first_use() -> None:
  # The naming round and the summarize path each build one backend; the import
  # binds nothing and the module attribute resolves (and patch-pins) lazily
  # through the shared load_build_backend (src/agents/backends/deferred_build.py).
  code = (
      "import json, sys; "
      "import src.core.autonamer, src.core.recap; "
      f"before = sorted(set(sys.modules) & {set(SESSIONS_HEAVY_MODULES)!r}); "
      "resolved = callable(src.core.autonamer.build_backend) and callable(src.core.recap.build_backend); "
      "after = sorted(set(sys.modules) & {'src.agents.backends.registry'}); "
      "sys.stderr.write(json.dumps([before, resolved, after]))")
  proc = _run_probe(code)
  before, resolved, after = json.loads(proc.stderr)
  assert before == [], (
      "autonamer or recap pulled the backends stack at module import: {before}; "
      "the M99 server import floor (docs/perf_baseline.md) depends on the naming "
      "round and the summarize path loading it at their one build".format(before=before))
  assert resolved is True, "the lazy build_backend binding did not resolve through the module attribute"
  assert after == ["src.agents.backends.registry"], (f"the lazy binding loaded unexpected modules: {after}")
