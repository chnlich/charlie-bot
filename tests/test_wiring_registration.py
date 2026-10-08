"""A package's routes exist only because its line in PACKAGES ran its register().

The server and the CLI read the wiring registry, so deleting a package line must remove that
package's routes and leave the rest of the app intact. The registry refuses a second
registration of one command or service name, and register_all() fills it once per process.
"""

import json
import subprocess
import sys

import conftest
import pytest

import server
from src.app import registrations
from src.runtime.hooks import wiring

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
