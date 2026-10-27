"""The wiring registry: a package's registrations exist only while its line is in PACKAGES.

The registry refuses a second registration of one setup step, and register_all() fills it once per
process.
"""

import pytest

from src.app import registrations
from src.runtime.hooks import wiring


def test_a_second_registration_of_one_setup_step_raises() -> None:
  registrations.register_all()
  steps_before = wiring.setup_steps()

  with pytest.raises(ValueError, match="setup_step"):
    wiring.register_setup_step("src.features.cron.seed", attr="setup_step")
  with pytest.raises(ValueError, match="phase"):
    wiring.register_service("not_registered", "not_imported", phase="late")

  assert wiring.setup_steps() == steps_before


def test_register_all_twice_registers_once() -> None:
  routers_before, commands_before = wiring.routers(), wiring.commands()

  registrations.register_all()

  assert wiring.routers() == routers_before
  assert wiring.commands() == commands_before
