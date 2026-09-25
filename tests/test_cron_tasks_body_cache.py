"""The cron tasks poll's generation-keyed body cache (src/api/cron.py):
one render + one deflate per config generation, every poll in between
serving both cached bodies, and a config change re-rendering once."""

from __future__ import annotations

import gzip
import json

import pytest
from conftest import _page_request, assert_gzip_served

import src.api.cron as cron_mod
from src.api.cron import list_cron_tasks
from src.core.config import ScheduledTaskConfig


@pytest.fixture(autouse=True)
def _fresh_cron_body_cache(monkeypatch: pytest.MonkeyPatch) -> None:
  """The module-level cache persists across tests; every test starts empty."""
  monkeypatch.setattr(cron_mod, "_CRON_TASKS_BODY_CACHE", None)


@pytest.mark.asyncio
async def test_cron_tasks_gzip_ships_precompressed_body(monkeypatch: pytest.MonkeyPatch) -> None:
  """A gzip-accepting poll serves the cached gzip form: the decompressed bytes
  equal the plain body, the vary header names the negotiator, and the parsed
  payload keeps the task-list shape (prompt excluded, the M46 dump contract)."""
  tasks = [
      ScheduledTaskConfig(name="nightly", cron="* * * * *", prompt="nightly prompt", backend="codex-o3")
  ]
  monkeypatch.setattr(cron_mod, "get_scheduled_tasks", lambda: tasks)
  monkeypatch.setattr(cron_mod, "get_scheduled_task_errors", list)

  gz = await list_cron_tasks(_page_request("gzip"))
  plain = await list_cron_tasks(_page_request())

  assert_gzip_served(gz)
  assert gzip.decompress(gz.body) == plain.body
  payload = json.loads(plain.body)
  assert [row["name"] for row in payload] == ["nightly"]
  assert "prompt" not in payload[0]


@pytest.mark.asyncio
async def test_cron_tasks_repeat_serves_cache_without_rerender(monkeypatch: pytest.MonkeyPatch) -> None:
  """A repeat poll of the same generation serves both cached bodies and
  re-renders nothing."""
  tasks = [ScheduledTaskConfig(name="nightly", cron="* * * * *", prompt="p", backend="codex-o3")]
  monkeypatch.setattr(cron_mod, "get_scheduled_tasks", lambda: tasks)
  monkeypatch.setattr(cron_mod, "get_scheduled_task_errors", list)
  first = await list_cron_tasks(_page_request("gzip"))

  def explode(content: object) -> bytes:
    raise AssertionError("repeat cron poll re-rendered the body")

  monkeypatch.setattr(cron_mod, "fast_json_bytes", explode)
  second = await list_cron_tasks(_page_request("gzip"))
  assert second.body == first.body


@pytest.mark.asyncio
async def test_cron_tasks_generation_change_rerenders(monkeypatch: pytest.MonkeyPatch) -> None:
  """A config change rebuilds the snapshot's tasks list: the next poll
  re-renders once and its body carries the new task."""
  current = [ScheduledTaskConfig(name="old", cron="* * * * *", prompt="p", backend="codex-o3")]
  fresh = [ScheduledTaskConfig(name="new", cron="* * * * *", prompt="p", backend="codex-o3")]
  monkeypatch.setattr(cron_mod, "get_scheduled_tasks", lambda: current)
  monkeypatch.setattr(cron_mod, "get_scheduled_task_errors", list)
  first = await list_cron_tasks(_page_request("gzip"))

  current = fresh
  second = await list_cron_tasks(_page_request("gzip"))

  assert second.body != first.body
  assert json.loads(gzip.decompress(second.body))[0]["name"] == "new"


@pytest.mark.asyncio
async def test_cron_tasks_plain_request_stays_uncompressed(monkeypatch: pytest.MonkeyPatch) -> None:
  """A client sending no Accept-Encoding reads the plain body: no
  Content-Encoding header."""
  monkeypatch.setattr(cron_mod, "get_scheduled_tasks", list)
  monkeypatch.setattr(cron_mod, "get_scheduled_task_errors", list)

  plain = await list_cron_tasks(_page_request())

  assert "content-encoding" not in plain.headers
