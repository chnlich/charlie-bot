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

import server
from src.app import registrations
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

  with pytest.raises(ValueError, match=command):
    wiring.register_command(command, "src.features.memory.cli")
  with pytest.raises(ValueError, match=service):
    wiring.register_service(service, "src.features.cron.service")
  with pytest.raises(ValueError, match="phase"):
    wiring.register_service("not_registered", "src.features.cron.service", phase="late")

  assert wiring.commands() == commands_before
  assert "not_registered" not in [name for name, _ in wiring.service_stops()]


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
