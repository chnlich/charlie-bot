"""The chat-thread subtree (every session whose parent chain reaches a Slack/Discord
thread session, the thread session itself included) splits the sidebar's Workspace
listing from the new Threads listing.

Workspace (route + homepage render + auto-redirect) returns no chat-thread rows —
the thread session, its task descendants, and its projected legacy worker-thread
leaf all stay out — while ``GET /api/sessions/chat-threads`` returns exactly that
subtree's active rows with the Workspace row shape. The two routes partition the
active non-cron rows, and the cron-subtree rule beside which the walk lives keeps
its results unchanged.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    apply_config_overrides,
    build_env,
    create_task,
    make_legacy_cron_session,
    page_initial_sessions,
    seed_thread,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_config_on_loop, get_session_manager, get_task_manager, get_thread_manager
from src.api.pages import router as pages_router
from src.api.sessions import project_worker_threads, router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.models import (
    CreateSessionRequest,
    DiscordOrigin,
    SessionMetadata,
    SessionStatus,
    SlackOrigin,
)
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from src.core.threads import ThreadManager


@dataclass
class Fixture:
  """One corpus exercising every chat-thread classification the sidebar lists make."""

  cfg: CharlieBotConfig
  session_mgr: SessionManager
  tree: TaskTreeManager
  thread_mgr: ThreadManager
  thread: SessionMetadata  # active discord-origin session (the subtree's root)
  slack_thread: SessionMetadata  # active slack-origin session (the other origin field)
  child: SessionMetadata  # active task node whose parent is the thread session
  grandchild: SessionMetadata  # active worker under the child
  thread_legacy: SessionMetadata  # the thread session's projected worker-leaf row
  archived_thread: SessionMetadata  # archived discord-origin session (neither list)
  cron: SessionMetadata  # active cron session (scheduled_task set)
  cron_child: SessionMetadata  # active worker child of the cron session
  plain: SessionMetadata  # active plain session (Workspace's own row)


async def _build_fixture(tmp_path: Path) -> Fixture:
  cfg, session_mgr, tree = build_env(tmp_path)
  thread_mgr = ThreadManager(cfg)
  thread = await session_mgr.create_session(
      CreateSessionRequest(
          name="Discord #general 2026",
          discord_origin=DiscordOrigin(guild_id="g1", parent_channel_id="c1", thread_id="t1"),
          group="Discord #general"),
      backend=OPUS_BACKEND_ID)
  slack_thread = await session_mgr.create_session(
      CreateSessionRequest(
          name="Slack #general 2026",
          slack_origin=SlackOrigin(team_id="T1", channel_id="C1", thread_ts="1700000000.000100")),
      backend=OPUS_BACKEND_ID)
  child = await create_task(tree, parent=thread.id, request_id="th-child-1", profile="manager", name="thread child")
  grandchild = await create_task(
      tree, parent=child.id, request_id="th-child-2", profile="worker", name="thread grandchild")
  thread_legacy = await seed_thread(thread_mgr, thread, "thread worker leaf")
  archived_thread = await session_mgr.create_session(
      CreateSessionRequest(
          name="Discord #general archived",
          discord_origin=DiscordOrigin(guild_id="g1", parent_channel_id="c1", thread_id="t2"),
          group="Discord #general"),
      backend=OPUS_BACKEND_ID)
  await session_mgr.archive_session(archived_thread.id)
  cron = await make_legacy_cron_session(session_mgr, "nightly")
  cron_child = await create_task(tree, parent=cron.id, request_id="cron-leaf-1", profile="worker", name="cron leaf")
  plain = await session_mgr.create_session(CreateSessionRequest(name="Plain"), backend=OPUS_BACKEND_ID)
  return Fixture(
      cfg=cfg,
      session_mgr=session_mgr,
      tree=tree,
      thread_mgr=thread_mgr,
      thread=thread,
      slack_thread=slack_thread,
      child=child,
      grandchild=grandchild,
      thread_legacy=thread_legacy,
      archived_thread=archived_thread,
      cron=cron,
      cron_child=cron_child,
      plain=plain)


# Every row the chat-thread rule claims, by fixture name (statuses never matter
# to the walk; the active-only listings drop the archived one on their own).
_CHAT_THREAD_ROWS = ("thread", "slack_thread", "child", "grandchild", "thread_legacy", "archived_thread")


def _thread_ids(fx: Fixture) -> set[str]:
  return {getattr(fx, name).id for name in _CHAT_THREAD_ROWS}


def _api_client(fx: Fixture) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config_on_loop] = lambda: fx.cfg
  app.dependency_overrides[get_session_manager] = lambda: fx.session_mgr
  app.dependency_overrides[get_task_manager] = lambda: fx.tree
  app.dependency_overrides[get_thread_manager] = lambda: fx.thread_mgr
  return TestClient(app)


def _page_client(fx: Fixture) -> TestClient:
  app = FastAPI()
  app.include_router(pages_router)
  apply_config_overrides(app, fx.cfg)
  app.dependency_overrides[get_session_manager] = lambda: fx.session_mgr
  app.dependency_overrides[get_task_manager] = lambda: fx.tree
  app.dependency_overrides[get_thread_manager] = lambda: fx.thread_mgr
  return TestClient(app)


@pytest.mark.asyncio
async def test_subtree_walk_maps_the_chat_thread_subtree_and_spares_the_cron_one(tmp_path: Path) -> None:
  fx = await _build_fixture(tmp_path)
  chat = await fx.session_mgr.chat_thread_subtree_roots()
  # Both origin fields root a subtree, and the root itself is a member.
  assert chat[fx.thread.id] == fx.thread.id
  assert chat[fx.slack_thread.id] == fx.slack_thread.id
  assert chat[fx.child.id] == fx.thread.id
  assert chat[fx.grandchild.id] == fx.thread.id
  assert chat[fx.archived_thread.id] == fx.archived_thread.id  # statuses never matter
  assert not (set(chat) & {fx.cron.id, fx.cron_child.id, fx.plain.id})

  # The cron rule beside which the walk lives keeps its results unchanged: the
  # cron child maps to its cron session, and no chat-thread row joins it.
  cron = await fx.session_mgr.cron_subtree_roots()
  assert cron[fx.cron_child.id] == fx.cron.id
  assert fx.cron.id not in cron
  assert not (set(cron) & _thread_ids(fx))


@pytest.mark.asyncio
async def test_workspace_and_threads_partition_the_active_rows_with_one_row_shape(tmp_path: Path) -> None:
  fx = await _build_fixture(tmp_path)
  client = _api_client(fx)
  workspace_ids = {row["id"] for row in client.get("/api/sessions/").json()}
  threads_rows = client.get("/api/sessions/chat-threads").json()
  threads_ids = {row["id"] for row in threads_rows}

  # Workspace: the plain row stays, the whole chat-thread subtree rides no listing.
  assert fx.plain.id in workspace_ids
  assert not (workspace_ids & _thread_ids(fx))
  assert fx.cron.id not in workspace_ids and fx.cron_child.id not in workspace_ids

  # Threads: exactly the subtree's active rows, the projected legacy worker
  # leaf among them; the archived thread session stays out.
  assert threads_ids == _thread_ids(fx) - {fx.archived_thread.id}
  assert fx.plain.id not in threads_ids

  # The two routes partition the active non-cron rows, and the union is the
  # Workspace listing the cron exclusion alone produced before the split — the
  # projected legacy worker-thread leaves included.
  cron_subtree = await fx.session_mgr.cron_subtree_roots()
  unprojected = [
      row for row in await fx.session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=False)
      if row.id not in cron_subtree
  ]
  active = {row.id for row in await project_worker_threads(unprojected, fx.cfg, fx.thread_mgr)}
  assert not (workspace_ids & threads_ids)
  assert workspace_ids | threads_ids == active

  # One row shape: the Threads route serves the Workspace dump's key set byte
  # for byte (worker-leaf projection, derived sidebar state, schedule join).
  workspace_keys = {row["id"]: set(row) for row in client.get("/api/sessions/").json()}
  for row in threads_rows:
    assert set(row) == workspace_keys[fx.plain.id]


@pytest.mark.asyncio
async def test_threads_route_memos_never_serve_the_workspace_body(tmp_path: Path) -> None:
  import src.api.sessions as sessions_api

  fx = await _build_fixture(tmp_path)
  client = _api_client(fx)
  workspace = client.get("/api/sessions/")
  threads = client.get("/api/sessions/chat-threads")
  # A repeat of each route (the other route's memo now warm) still serves its
  # own body, byte-identical, from its own memo slots.
  assert client.get("/api/sessions/").content == workspace.content
  assert client.get("/api/sessions/chat-threads").content == threads.content
  assert workspace.content != threads.content

  # The slots themselves are per-route: dropping one route's whole-body slot
  # forces only that route's re-render, and the bytes come back identical.
  sessions_api._workspace_list_memos.whole_body = None
  sessions_api._chat_threads_list_memos.whole_body = None
  assert client.get("/api/sessions/").content == workspace.content
  assert client.get("/api/sessions/chat-threads").content == threads.content


@pytest.mark.asyncio
async def test_homepage_initial_list_and_redirect_skip_the_chat_thread_subtree(tmp_path: Path) -> None:
  fx = await _build_fixture(tmp_path)
  page_client = _page_client(fx)
  redirect = page_client.get("/", follow_redirects=False)
  assert redirect.status_code in (301, 302, 307)
  assert redirect.headers["location"].split("session=")[1] not in _thread_ids(fx)
  row_ids = {row["id"] for row in page_initial_sessions(page_client, fx.plain.id)}
  assert fx.plain.id in row_ids
  assert not (row_ids & _thread_ids(fx))
