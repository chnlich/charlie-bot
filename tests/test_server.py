"""Tests for server.py utility functions."""

import json
import pytest
from pathlib import Path

from src.core.config import load_config


# Import the private helper without importing the full FastAPI app
# (avoids triggering lifespan, static file mounts, etc.)
import importlib.util, sys


def _get_find_events_file():
    """Import _find_events_file from server without triggering app startup."""
    spec = importlib.util.spec_from_file_location(
        "server_module",
        Path(__file__).parent.parent / "server.py",
    )
    mod = importlib.util.module_from_spec(spec)
    # Inject the module under a private name to avoid clobbering 'server'
    sys.modules["server_module"] = mod
    spec.loader.exec_module(mod)
    return mod._find_events_file


class TestFindEventsFile:
    def test_returns_none_when_sessions_dir_missing(self, tmp_home):
        cfg = load_config()
        find = _get_find_events_file()
        # sessions_dir does not exist
        result = find("any-thread-id", cfg)
        assert result is None

    def test_returns_none_when_thread_not_found(self, tmp_home):
        cfg = load_config()
        cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
        find = _get_find_events_file()
        result = find("ghost-thread-id", cfg)
        assert result is None

    def test_finds_existing_events_file(self, tmp_home):
        cfg = load_config()
        thread_id = "thread-abc123"
        events_path = cfg.sessions_dir / "sess-1" / "threads" / thread_id / "data" / "events.jsonl"
        events_path.parent.mkdir(parents=True, exist_ok=True)
        events_path.write_text('{"type":"output"}\n')

        find = _get_find_events_file()
        result = find(thread_id, cfg)
        assert result == events_path

    def test_searches_across_multiple_sessions(self, tmp_home):
        cfg = load_config()
        thread_id = "thread-xyz"
        # Put the events file in the second session directory
        events_path = cfg.sessions_dir / "sess-2" / "threads" / thread_id / "data" / "events.jsonl"
        events_path.parent.mkdir(parents=True, exist_ok=True)
        events_path.write_text('{"type":"output"}\n')
        # First session exists but doesn't have this thread
        (cfg.sessions_dir / "sess-1").mkdir(parents=True, exist_ok=True)

        find = _get_find_events_file()
        result = find(thread_id, cfg)
        assert result == events_path

    def test_returns_path_even_if_file_missing(self, tmp_home):
        """
        _find_events_file returns the candidate path only if it exists.
        Verify it does NOT return a non-existent path.
        """
        cfg = load_config()
        thread_id = "missing-thread"
        # Create thread dir but NOT the events.jsonl file
        thread_dir = cfg.sessions_dir / "sess-1" / "threads" / thread_id / "data"
        thread_dir.mkdir(parents=True, exist_ok=True)

        find = _get_find_events_file()
        result = find(thread_id, cfg)
        assert result is None

    def test_ignores_non_directory_entries_in_sessions_dir(self, tmp_home):
        cfg = load_config()
        cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
        # Create a file (not a directory) inside sessions_dir
        (cfg.sessions_dir / "somefile.txt").write_text("noise")

        find = _get_find_events_file()
        result = find("any-id", cfg)
        assert result is None
