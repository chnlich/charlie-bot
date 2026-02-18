"""Tests for src/core/config.py."""

import os
import pytest
from pathlib import Path
from unittest.mock import patch

import src.core.config as cfg_mod
from src.core.config import CharliBotConfig, load_config, get_config


class TestCharliBotConfig:
    def test_default_values(self, tmp_home):
        cfg = load_config()
        assert cfg.gemini_model == "gemini-2.0-flash"
        assert cfg.kimi_model == "moonshot-v1-8k"
        assert cfg.max_concurrent_workers == 3
        assert cfg.worker_timeout_seconds == 3600

    def test_charliebot_home_from_env(self, tmp_home):
        cfg = load_config()
        assert cfg.charliebot_home == tmp_home

    def test_property_paths(self, tmp_home):
        cfg = load_config()
        assert cfg.sessions_dir == tmp_home / "sessions"
        assert cfg.backups_dir == tmp_home / "backups"
        assert cfg.logs_dir == tmp_home / "logs"
        assert cfg.repos_dir == tmp_home / "repos"
        assert cfg.memory_file == tmp_home / "MEMORY.md"
        assert cfg.past_tasks_file == tmp_home / "PAST_TASKS.md"
        assert cfg.progress_file == tmp_home / "PROGRESS.md"
        assert cfg.config_file == tmp_home / "config.yaml"

    def test_load_config_from_yaml(self, tmp_home):
        import yaml
        config_file = tmp_home / "config.yaml"
        config_file.write_text(yaml.dump({"max_concurrent_workers": 5}))
        cfg = load_config()
        assert cfg.max_concurrent_workers == 5

    def test_get_config_singleton(self, tmp_home):
        c1 = get_config()
        c2 = get_config()
        assert c1 is c2

    def test_get_config_reset_on_new_env(self, tmp_home):
        # Fixture resets _config, so get_config should create a fresh one
        cfg = get_config()
        assert cfg is not None
        assert cfg.charliebot_home == tmp_home

    def test_env_var_override(self, tmp_home):
        with patch.dict(os.environ, {"CHARLIEBOT_MAX_CONCURRENT_WORKERS": "10"}):
            cfg_mod._config = None
            cfg = load_config()
            assert cfg.max_concurrent_workers == 10
        cfg_mod._config = None
