"""A package's routes exist only because its line in PACKAGES ran its register().

The server and the CLI read the wiring registry, so deleting a package line must remove that
package's routes and leave the rest of the app intact. The registry refuses a second
registration of one command or service name, and register_all() fills it once per process.
The page-render registry follows the same rule: a package's pages, templates and template globals
exist only while its line is in PACKAGES.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

import conftest
import pytest
import uvicorn

import server
from src.app import registrations
from src.infra.config import CharlieBotConfig
from src.runtime import templating
from src.runtime.hooks import page_render, wiring

DELETED_PACKAGE = "src.features.latex"
DELETED_PREFIX = "/api/latex"

# The probe process deletes one package line, registers, then imports the server module,
# which calls register_all() again (a no-op) and assembles the app from the registry.
PROBE = f"""
import json
from src.app import registrations
registrations.PACKAGES = tuple(p for p in registrations.PACKAGES if p != {DELETED_PACKAGE!r})
registrations.register_all()
import server
print(json.dumps([route.path for route in server.app.routes]))
"""


def test_deleting_a_package_line_removes_its_routes_and_keeps_the_others() -> None:
  probe = subprocess.run(
      [sys.executable, "-c", PROBE], cwd=conftest.ROOT, capture_output=True, text=True, timeout=60, check=False)
  assert probe.returncode == 0, probe.stderr
  without_package = json.loads(probe.stdout.splitlines()[-1])
  with_package = [route.path for route in server.app.routes]

  assert any(path.startswith(DELETED_PREFIX) for path in with_package)
  assert not any(path.startswith(DELETED_PREFIX) for path in without_package)
  assert without_package == [path for path in with_package if not path.startswith(DELETED_PREFIX)]


def test_a_second_registration_of_one_name_raises() -> None:
  command = next(iter(wiring.commands()))
  service = next(name for name, _ in wiring.service_starts("ready"))
  commands_before = wiring.commands()
  views_before = wiring.file_views()
  checks_before = wiring.startup_checks()
  roots_before = wiring.diff_roots()

  with pytest.raises(ValueError, match=command):
    wiring.register_command(command, "src.features.memory.cli")
  with pytest.raises(ValueError, match="serve_artifact_path"):
    wiring.register_file_view("src.features.artifacts.artifact_view", attr="serve_artifact_path")
  with pytest.raises(ValueError, match="check_backend_refs"):
    wiring.register_startup_check("src.features.cron.backend_refs", attr="check_backend_refs")
  with pytest.raises(ValueError, match="memory_dir"):
    wiring.register_diff_root("src.features.memory.store_root", attr="memory_dir")
  with pytest.raises(ValueError, match=service):
    wiring.register_service(service, "src.features.cron.service")
  with pytest.raises(ValueError, match="phase"):
    wiring.register_service("not_registered", "src.features.cron.service", phase="late")

  assert wiring.commands() == commands_before
  assert wiring.file_views() == views_before
  assert wiring.startup_checks() == checks_before
  assert wiring.diff_roots() == roots_before
  assert "not_registered" not in [name for name, _ in wiring.service_stops()]


def _run_server_main(
    monkeypatch: pytest.MonkeyPatch, cfg: CharlieBotConfig, events: list[str], check_error: ValueError | None) -> None:
  """Run server.main() against *cfg* with one recording startup check and a recording uvicorn.run;
  *events* receives their calls in run order."""

  def recording_check(_cfg: CharlieBotConfig) -> None:
    events.append("startup_check")
    if check_error is not None:
      raise check_error

  monkeypatch.setattr(server, "apply_agent_environment", lambda: None)
  monkeypatch.setattr(server, "get_config", lambda: cfg)
  monkeypatch.setattr(server.wiring, "startup_checks", lambda: [recording_check])
  monkeypatch.setattr(uvicorn, "run", lambda *_args, **_kwargs: events.append("uvicorn.run"))
  server.main()


def test_server_main_stops_on_an_empty_backend_catalog_before_any_startup_check(
    monkeypatch: pytest.MonkeyPatch) -> None:
  events: list[str] = []

  with pytest.raises(ValueError, match=r"backends\.options"):
    _run_server_main(monkeypatch, CharlieBotConfig(), events, None)

  assert events == []


def test_server_main_serves_only_after_every_startup_check_passes(monkeypatch: pytest.MonkeyPatch) -> None:
  option = conftest.backend_option(id="claude-opus", label="Opus", type="cc-claude", model="m")
  cfg = CharlieBotConfig(backends={"options": [option]})
  passed: list[str] = []
  stopped: list[str] = []

  _run_server_main(monkeypatch, cfg, passed, None)
  with pytest.raises(ValueError, match="stop the start"):
    _run_server_main(monkeypatch, cfg, stopped, ValueError("stop the start"))

  assert passed == ["startup_check", "uvicorn.run"]
  assert stopped == ["startup_check"]


def test_register_all_twice_registers_once() -> None:
  routers_before, commands_before = wiring.routers(), wiring.commands()

  registrations.register_all()

  assert wiring.routers() == routers_before
  assert wiring.commands() == commands_before


def test_services_stop_in_reverse_start_order_ready_ones_first() -> None:
  early = [name for name, _ in wiring.service_starts("early")]
  ready = [name for name, _ in wiring.service_starts("ready")]

  assert early and ready
  assert [name for name, _ in wiring.service_stops()] == [*reversed(ready), *reversed(early)]


# The probe process deletes one package line, registers, then renders /diff through the real app.
DIFF_PROBE = """
import json
from src.app import registrations
registrations.PACKAGES = tuple(p for p in registrations.PACKAGES if p != {deleted!r})
registrations.register_all()
import server
from fastapi.testclient import TestClient
from src.runtime import templating
response = TestClient(server.app).get("/diff")
print(json.dumps({{
    "status": response.status_code,
    "button": "open-codeserver" in response.text,
    "global": "code_server_enabled" in templating.templates().env.globals,
}}))
"""


def probe_diff_page(deleted_package: str, home: Path) -> dict:
  probe = subprocess.run(
      [sys.executable, "-c", DIFF_PROBE.format(deleted=deleted_package)],
      cwd=conftest.ROOT,
      capture_output=True,
      text=True,
      timeout=60,
      check=False,
      env={
          **os.environ, "CHARLIEBOT_HOME": str(home)
      })
  assert probe.returncode == 0, probe.stderr
  return json.loads(probe.stdout.splitlines()[-1])


def test_the_diff_page_renders_without_the_code_server_package(tmp_path: Path) -> None:
  without_package = probe_diff_page("src.features.code_server", tmp_path / "home")

  assert without_package == {"status": 200, "button": False, "global": False}
  assert "code_server_enabled" in templating.templates().env.globals


# The probe process deletes the artifacts line, registers, then asks the real app for a session artifact page.
ARTIFACT_PAGE_PROBE = """
import json
from src.app import registrations
registrations.PACKAGES = tuple(p for p in registrations.PACKAGES if p != "src.features.artifacts")
registrations.register_all()
import server
from fastapi.testclient import TestClient
response = TestClient(server.app).get("/absolute_filepath" + {page!r})
print(json.dumps({{"status": response.status_code, "body": response.text}}))
"""


def test_a_session_artifact_page_is_a_plain_file_without_the_artifacts_package(tmp_path: Path) -> None:
  home = tmp_path / "home"
  page = home / "sessions" / "S" / "artifacts" / "plan_01.html"
  page.parent.mkdir(parents=True)
  page_html = "<html><body><h1>Plan</h1></body></html>"
  page.write_text(page_html, encoding="utf-8")

  probe = subprocess.run(
      [sys.executable, "-c", ARTIFACT_PAGE_PROBE.format(page=str(page))],
      cwd=conftest.ROOT,
      capture_output=True,
      text=True,
      timeout=60,
      check=False,
      env={
          **os.environ, "CHARLIEBOT_HOME": str(home)
      })

  assert probe.returncode == 0, probe.stderr
  assert json.loads(probe.stdout.splitlines()[-1]) == {"status": 200, "body": page_html}


def test_a_second_registration_of_one_template_global_raises() -> None:
  name, (module, _) = next(iter(page_render.template_globals().items()))
  globals_before = page_render.template_globals()

  with pytest.raises(ValueError, match=name):
    page_render.register_template_global(name, module, attr="other")

  assert page_render.template_globals() == globals_before


def test_one_template_name_in_two_directories_raises(tmp_path: Path) -> None:
  first, second = tmp_path / "first", tmp_path / "second"
  for directory in (first, second):
    (directory / "partials").mkdir(parents=True)
    (directory / "partials" / "card.html").write_text("x", encoding="utf-8")

  with pytest.raises(ValueError, match=r"partials/card\.html"):
    templating._assert_unique_template_names([first, second])


def test_every_registered_template_directory_holds_no_name_of_another() -> None:
  templating._assert_unique_template_names(templating._template_directories())
