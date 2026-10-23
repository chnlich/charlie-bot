"""A stop ends the server process at ``server.main``, not in interpreter finalization.

uvicorn returns from ``uvicorn.run`` once a Ctrl-C stop completes, and finalizing the server's heap then
spends 19.7-21.4 s in ``_Py_Finalize > finalize_modules > gc_collect_main`` (measured on a copy of the
production data), so ``main`` flushes its writers, shuts logging down and calls ``os._exit(0)``. The child
below is the real ``main`` with a stand-in ``uvicorn.run``: stdout and stderr share one file, as the
launcher's ``tee`` gives them, and an atexit callback proves the process never finalized.
"""

import os
import pathlib
import subprocess
import sys

import conftest

_CHILD = """
import atexit
import logging
import logging.config
import pathlib
import sys

home, marker = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])


def _mark() -> None:
  marker.write_text("finalized", encoding="utf-8")


atexit.register(_mark)

import uvicorn
from src.infra import config
from uvicorn import config as uvicorn_config

import server

cfg = config.CharlieBotConfig(charliebot_home=home)
server.config.get_config = lambda: cfg
server.agent_environment.apply_agent_environment = lambda: None
server.config.require_backends = lambda _cfg: None
server.v1_sessions.require_no_v1_sessions = lambda _cfg: None
server.wiring.startup_checks = lambda: []


def _run(app, **kwargs) -> None:
  logging.config.dictConfig(uvicorn_config.LOGGING_CONFIG)
  server.log.info("charliebot_shutdown", shutdown_ms=1)
  logging.getLogger("uvicorn.error").info("uvicorn stand-in line")


uvicorn.run = _run
server.main()
"""


def test_main_exits_after_uvicorn_returns_and_writes_both_log_lines(tmp_path: pathlib.Path) -> None:
  home = tmp_path / "home"
  home.mkdir()
  marker = tmp_path / "finalized.marker"
  log = tmp_path / "server.log"
  env = {**os.environ, "CHARLIEBOT_HOME": str(home)}
  env.pop("PYTHONUNBUFFERED", None)

  with log.open("wb") as log_file:
    result = subprocess.run(
        [sys.executable, "-c", _CHILD, str(home), str(marker)],
        cwd=conftest.ROOT,
        env=env,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        timeout=60)

  written = log.read_text(encoding="utf-8")
  assert "charliebot_shutdown" in written
  assert "uvicorn stand-in line" in written
  assert result.returncode == 0
  assert not marker.exists(), "the process finalized instead of exiting in main"
