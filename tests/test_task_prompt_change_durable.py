"""The prompt-edit crash boundary: the metadata write and the prompt_changed
fact are two durable writes, and a crash between them is repaired without a
workflow state machine — retries and repeated recovery land exactly one fact
per edit, never lose one, and never overwrite a concurrent task mutation."""

from __future__ import annotations

import pathlib

import conftest
import pytest

from src.core import event_types as ET
from src.core import models, task_sessions

pytestmark = pytest.mark.asyncio


def prompt_facts(tree: task_sessions.TaskTreeManager, session_id: str) -> list[dict]:
  return [e for e in tree.events.load_events(session_id) if e.get("type") == ET.PROMPT_CHANGED]


async def _leaf(tmp_path: pathlib.Path):
  cfg, _session_mgr, tree = conftest.build_env(tmp_path)
  meta = await tree.create_task(
      request_id="leaf",
      task_parent_id=None,
      profile="worker",
      task=models.TaskSpec(goal="g"),
      name="L",
      backend=None,
      caller=conftest.OPERATOR)
  return cfg, tree, meta.id


async def test_successful_patch_is_durable_across_both_scopes(tmp_path: pathlib.Path) -> None:
  _cfg, tree, sid = await _leaf(tmp_path)
  meta = await tree.patch_task(
      sid,
      models.PatchSessionTaskRequest(subtree_prompt="subtree rule", node_prompt="node rule"),
      caller=conftest.OPERATOR)
  assert meta.subtree_prompt_ref and meta.node_prompt_ref
  facts = prompt_facts(tree, sid)
  assert [(f["scope"], f["previous_ref"], f["new_ref"]) for f in facts] == [
      ("subtree", None, meta.subtree_prompt_ref),
      ("node", None, meta.node_prompt_ref),
  ]
  # The metadata owner holds the refs; the bodies are immutable content.
  for ref in (meta.subtree_prompt_ref, meta.node_prompt_ref):
    body = (tree._cfg.charliebot_home / "prompt_bodies" / f"{ref}.md").read_text(encoding="utf-8")
    assert body in ("subtree rule", "node rule")


async def test_two_scope_patch_crash_between_scopes_repairs_cleanly(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, tree, sid = await _leaf(tmp_path)
  real_save = tree._save_meta
  calls = {"n": 0}

  async def failing_save(meta) -> None:
    # Fail the metadata save after the subtree scope applied but before the
    # node scope did (the second save inside the same PATCH).
    calls["n"] += 1
    if calls["n"] == 1 and meta.subtree_prompt_ref is not None:
      raise OSError("injected mid-patch failure")
    await real_save(meta)

  monkeypatch.setattr(tree, "_save_meta", failing_save)
  with pytest.raises(OSError):
    await tree.patch_task(
        sid,
        models.PatchSessionTaskRequest(subtree_prompt="subtree rule", node_prompt="node rule"),
        caller=conftest.OPERATOR)
  monkeypatch.setattr(tree, "_save_meta", real_save)
  meta = await tree.patch_task(
      sid,
      models.PatchSessionTaskRequest(subtree_prompt="subtree rule", node_prompt="node rule"),
      caller=conftest.OPERATOR)
  facts = prompt_facts(tree, sid)
  assert [(f["scope"], f["new_ref"]) for f in facts] == [
      ("subtree", meta.subtree_prompt_ref), ("node", meta.node_prompt_ref)
  ]


async def test_recovery_sweep_is_idempotent_and_does_not_duplicate(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, sid = await _leaf(tmp_path)
  # Simulate the crash window: metadata swapped, fact append failed.
  real_ensure = tree._ensure_prompt_changed_fact

  async def failing_ensure(session_id: str, meta, scope: str) -> None:
    if scope == "subtree" and meta.subtree_prompt_ref is not None:
      raise OSError("injected")
    await real_ensure(session_id, meta, scope)

  monkeypatch.setattr(tree, "_ensure_prompt_changed_fact", failing_ensure)
  with pytest.raises(OSError):
    await tree.patch_task(sid, models.PatchSessionTaskRequest(subtree_prompt="rule"), caller=conftest.OPERATOR)
  monkeypatch.setattr(tree, "_ensure_prompt_changed_fact", real_ensure)
  # Repeated recovery (the startup sweep) lands the fact once, then no-ops.
  from src.core import task_recovery
  await task_recovery.reconcile_task_tree(cfg, tree)
  first = prompt_facts(tree, sid)
  assert len(first) == 1
  await task_recovery.reconcile_task_tree(cfg, tree)
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert prompt_facts(tree, sid) == first


async def test_concurrent_task_mutation_is_not_overwritten(tmp_path: pathlib.Path) -> None:
  """A prompt PATCH that also renames saves both under the same lock; a crash
  before the final save loses neither on retry."""
  _cfg, tree, sid = await _leaf(tmp_path)
  meta = await tree.patch_task(
      sid,
      models.PatchSessionTaskRequest(name="renamed", subtree_prompt="rule", node_prompt="node rule"),
      caller=conftest.OPERATOR)
  assert meta.name == "renamed"
  assert meta.subtree_prompt_ref and meta.node_prompt_ref
  # Retry with identical values: idempotent, no duplicate facts.
  facts_before = prompt_facts(tree, sid)
  meta = await tree.patch_task(
      sid,
      models.PatchSessionTaskRequest(name="renamed", subtree_prompt="rule", node_prompt="node rule"),
      caller=conftest.OPERATOR)
  assert prompt_facts(tree, sid) == facts_before
  assert meta.name == "renamed"
