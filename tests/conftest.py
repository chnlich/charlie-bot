"""Shared fixtures for CharlieBot tests."""

import os
import pytest
from pathlib import Path
from unittest.mock import patch


@pytest.fixture()
def tmp_home(tmp_path):
    """
    Provide a temporary ~/.charliebot-style home directory and override
    CHARLIEBOT_HOME so all config/singleton calls use it.
    Resets the config singleton before and after each test.
    """
    home = tmp_path / ".charliebot"
    home.mkdir()

    # Reset the singleton before patching
    import src.core.config as cfg_mod
    cfg_mod._config = None

    with patch.dict(os.environ, {"CHARLIEBOT_HOME": str(home)}, clear=False):
        yield home

    # Reset the singleton again after the test
    cfg_mod._config = None
