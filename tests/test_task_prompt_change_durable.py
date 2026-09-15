"""The prompt-edit crash boundary: the metadata write and the prompt_changed
fact are two durable writes, and a crash between them is repaired without a
workflow state machine — retries and repeated recovery land exactly one fact
per edit, never lose one, and never overwrite a concurrent task mutation."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import make_home_config

from src.core import event_types as ET
from src.core.models import PatchSessionTaskRequest, TaskSpec
from src.core.run_token import CallerIdentity
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

pytestmark = pytest.mark.asyncio

OPERATOR = CallerIdentity(kind="operator")


def build_env(tmp_path: Path) -> tuple[object, TaskTreeManager]:
  cfg = make_home_config(tmp_path)
  return cfg, TaskTreeManager(cfg, SessionManager(cfg))


def prompt_facts(tree: TaskTreeManager, session_id: str) -> list[dict]:
  return [e for e in tree.events.load_events(session_id) if e.get("type") == ET.PROMPT_CHANGED]


async def _leaf(tmp_path: Path):
  cfg, tree = build_env(tmp_path)
  meta = await tree.create_task(
      request_id="leaf", task_parent_id=None, profile="worker",
      task=TaskSpec(goal="g"), name="L", backend=None, caller=OPERATOR)
  return cfg, tree, meta.id


async def test_successful_patch_is_durable_across_both_scopes(tmp_path: Path) -> None:
  _cfg, tree, sid = await _leaf(tmp_path)
  meta = await tree.patch_task(
      sid, PatchSessionTaskRequest(subtree_prompt="subtree rule", node_prompt="node rule"),
      caller=OPERATOR)
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


async def test_crash_after_metadata_save_before_fact_retry_lands_exactly_one_fact(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, sid = await _leaf(tmp_path)
  # Direction 1: the fact append fails after the metadata swap — the PATCH must
  # not report success, and the retry lands the one missing fact.
  real_ensure = tree._ensure_prompt_changed_fact
  calls = {"n": 0}

  async def failing_after_meta(session_id: str, meta, scope: str) -> None:
    # Fail exactly once: the post-edit ensure (the crash window — the metadata
    # already holds the new ref), never the entry repair a retry performs.
    if scope == "node" and getattr(meta, "node_prompt_ref") is not None and calls["n"] == 0:
      calls["n"] += 1
      raise OSError("injected event append failure")
    await real_ensure(session_id, meta, scope)

  monkeypatch.setattr(tree, "_ensure_prompt_changed_fact", failing_after_meta)
  with pytest.raises(OSError):
    await tree.patch_task(sid, PatchSessionTaskRequest(node_prompt="the rule"), caller=OPERATOR)
  assert prompt_facts(tree, sid) == []
  # The metadata swap survived (the edit is not lost), but the fact was missing.
  meta = await tree.load_meta(sid)
  assert meta.node_prompt_ref is not None
  # Retry (no failure injected): idempotent — exactly one fact, no duplicate.
  meta = await tree.patch_task(sid, PatchSessionTaskRequest(node_prompt="the rule"), caller=OPERATOR)
  facts = prompt_facts(tree, sid)
  assert [(f["scope"], f["new_ref"]) for f in facts] == [("node", meta.node_prompt_ref)]


async def test_crash_after_body_before_metadata_retry_does_not_lose_the_edit(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, sid = await _leaf(tmp_path)
  # Direction 2: the metadata save fails after the body is stored — the PATCH
  # fails visibly; the retry re-stores the same immutable body and completes.
  real_save = tree._save_meta
  save_calls = {"n": 0}

  async def failing_save(meta) -> None:
    save_calls["n"] += 1
    if save_calls["n"] == 1 and getattr(meta, "subtree_prompt_ref") is not None:
      raise OSError("injected metadata save failure")
    await real_save(meta)

  monkeypatch.setattr(tree, "_save_meta", failing_save)
  with pytest.raises(OSError):
    await tree.patch_task(sid, PatchSessionTaskRequest(subtree_prompt="the subtree rule"),
                          caller=OPERATOR)
  assert await tree.load_meta(sid) is not None
  meta = await tree.load_meta(sid)
  assert meta.subtree_prompt_ref is None  # the swap never landed
  # Retry: the same body content-addresses to the same ref; one edit, one fact.
  monkeypatch.setattr(tree, "_save_meta", real_save)
  meta = await tree.patch_task(sid, PatchSessionTaskRequest(subtree_prompt="the subtree rule"),
                               caller=OPERATOR)
  facts = prompt_facts(tree, sid)
  assert [(f["scope"], f["previous_ref"], f["new_ref"]) for f in facts] == [
      ("subtree", None, meta.subtree_prompt_ref)]
  # The immutable body store holds exactly one copy.
  bodies = list((cfg.charliebot_home / "prompt_bodies").glob("*.md"))
  assert len(bodies) == 1


async def test_two_scope_patch_crash_between_scopes_repairs_cleanly(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, sid = await _leaf(tmp_path)
  real_save = tree._save_meta
  calls = {"n": 0}

  async def failing_save(meta) -> None:
    # Fail the metadata save after the subtree scope applied but before the
    # node scope did (the second save inside the same PATCH).
    calls["n"] += 1
    if calls["n"] == 1 and getattr(meta, "subtree_prompt_ref") is not None:
      raise OSError("injected mid-patch failure")
    await real_save(meta)

  monkeypatch.setattr(tree, "_save_meta", failing_save)
  with pytest.raises(OSError):
    await tree.patch_task(
        sid, PatchSessionTaskRequest(subtree_prompt="subtree rule", node_prompt="node rule"),
        caller=OPERATOR)
  monkeypatch.setattr(tree, "_save_meta", real_save)
  meta = await tree.patch_task(
      sid, PatchSessionTaskRequest(subtree_prompt="subtree rule", node_prompt="node rule"),
      caller=OPERATOR)
  facts = prompt_facts(tree, sid)
  assert [(f["scope"], f["new_ref"]) for f in facts] == [
      ("subtree", meta.subtree_prompt_ref), ("node", meta.node_prompt_ref)]


async def test_recovery_sweep_is_idempotent_and_does_not_duplicate(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, sid = await _leaf(tmp_path)
  # Simulate the crash window: metadata swapped, fact append failed.
  real_ensure = tree._ensure_prompt_changed_fact

  async def failing_ensure(session_id: str, meta, scope: str) -> None:
    if scope == "subtree" and getattr(meta, "subtree_prompt_ref") is not None:
      raise OSError("injected")
    await real_ensure(session_id, meta, scope)

  monkeypatch.setattr(tree, "_ensure_prompt_changed_fact", failing_ensure)
  with pytest.raises(OSError):
    await tree.patch_task(sid, PatchSessionTaskRequest(subtree_prompt="rule"), caller=OPERATOR)
  monkeypatch.setattr(tree, "_ensure_prompt_changed_fact", real_ensure)
  # Repeated recovery (the startup sweep) lands the fact once, then no-ops.
  from src.core.task_recovery import reconcile_task_tree
  await reconcile_task_tree(cfg, tree, tree._sessions)
  first = prompt_facts(tree, sid)
  assert len(first) == 1
  await reconcile_task_tree(cfg, tree, tree._sessions)
  await reconcile_task_tree(cfg, tree, tree._sessions)
  assert prompt_facts(tree, sid) == first


async def test_later_edit_after_interrupted_edit_keeps_a_truthful_chain(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, sid = await _leaf(tmp_path)
  real_ensure = tree._ensure_prompt_changed_fact

  async def failing_ensure(session_id: str, meta, scope: str) -> None:
    if scope == "node" and getattr(meta, "node_prompt_ref") is not None:
      raise OSError("injected")
    await real_ensure(session_id, meta, scope)

  # Edit A→B lands in metadata; its fact is lost.
  monkeypatch.setattr(tree, "_ensure_prompt_changed_fact", failing_ensure)
  with pytest.raises(OSError):
    await tree.patch_task(sid, PatchSessionTaskRequest(node_prompt="rule B"), caller=OPERATOR)
  monkeypatch.setattr(tree, "_ensure_prompt_changed_fact", real_ensure)
  # A DIFFERENT edit (B→C) without a retry: the entry repair lands A→B first,
  # then B→C — no folded transition, no lost edit.
  meta = await tree.patch_task(sid, PatchSessionTaskRequest(node_prompt="rule C"), caller=OPERATOR)
  facts = prompt_facts(tree, sid)
  assert len(facts) == 2
  # The first fact ends at B's ref (the repaired A→B), the second starts from
  # it and ends at C — no folded transition, no lost edit.
  assert facts[0]["previous_ref"] is None
  assert facts[1]["previous_ref"] == facts[0]["new_ref"]
  assert facts[1]["new_ref"] == meta.node_prompt_ref
  assert facts[0]["new_ref"] != meta.node_prompt_ref


async def test_concurrent_task_mutation_is_not_overwritten(tmp_path: Path) -> None:
  """A prompt PATCH that also renames saves both under the same lock; a crash
  before the final save loses neither on retry."""
  cfg, tree, sid = await _leaf(tmp_path)
  meta = await tree.patch_task(
      sid,
      PatchSessionTaskRequest(name="renamed", subtree_prompt="rule", node_prompt="node rule"),
      caller=OPERATOR)
  assert meta.name == "renamed"
  assert meta.subtree_prompt_ref and meta.node_prompt_ref
  # Retry with identical values: idempotent, no duplicate facts.
  facts_before = prompt_facts(tree, sid)
  meta = await tree.patch_task(
      sid,
      PatchSessionTaskRequest(name="renamed", subtree_prompt="rule", node_prompt="node rule"),
      caller=OPERATOR)
  assert prompt_facts(tree, sid) == facts_before
  assert meta.name == "renamed"


async def test_next_launch_semantics_when_a_rule_is_edited_during_an_active_run(
        tmp_path: Path) -> None:
  """An edit during an active Run changes nothing for that Run; the next launch
  sees the new ref (the chain recheck picks it up at its own launch time)."""
  cfg, tree, sid = await _leaf(tmp_path)
  run_id = "active-run"
  import subprocess

  from src.core.models import RunRecord
  from src.core.runs import read_pid_stat
  await tree.runs.register_run(RunRecord(id=run_id, session_id=sid, kind="work"))
  proc = subprocess.Popen(["/bin/sleep", "60"])
  pair = read_pid_stat(proc.pid)
  await tree.runs.record_launch(sid, run_id, pid=proc.pid, pid_start=pair[0])
  try:
    before = (await tree.load_meta(sid)).node_prompt_ref
    await tree.patch_task(sid, PatchSessionTaskRequest(node_prompt="edited rule"), caller=OPERATOR)
    # The active Run's record is untouched; the node's ref moved for the next launch.
    run = await tree.runs.get_run(sid, run_id)
    assert run is not None and run.ended_at is None
    assert (await tree.load_meta(sid)).node_prompt_ref != before
  finally:
    proc.terminate()
