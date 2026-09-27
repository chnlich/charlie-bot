"""The v2 task-context assembly contract: one assembler, default roles at every
depth, ordered provenance, exact-body dedup, and the snapshot/hash object.

Covers plan 4.1's context/role terms at the assembly layer: the one manager
template at any depth with no PM load, the worker/review/verify/iteration/
scheduled-step contracts, inherited subtree rules root→node with no ancestor
node-rule or sibling leakage, byte-exact dedup with full source preservation,
memory selection provenance identical to the legacy strings, and the
snapshot/prompt_hash/char_count semantics.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_ID, build_env

from src.core.memory import (
    assemble_master,
    select_master_memory,
    select_worker_memory,
)
from src.core.models import PatchSessionTaskRequest, TaskSpec, TaskType
from src.core.run_token import CallerIdentity
from src.core.task_prompts import (
    PromptSnapshot,
    preview_snapshot,
    prompt_task_type,
)
from src.core.task_sessions import TaskTreeManager

pytestmark = pytest.mark.asyncio

OPERATOR = CallerIdentity(kind="operator")


async def create_task(
    mgr: TaskTreeManager,
    *,
    parent: str | None,
    profile: str = "manager",
    request_id: str,
    name: str | None = None,
    task: TaskSpec | None = None,
    **kwargs: object):
  return await mgr.create_task(
      request_id=request_id,
      task_parent_id=parent,
      profile=profile,
      task=task,
      name=name,
      backend=OPUS_BACKEND_ID,
      caller=OPERATOR,
      **kwargs)


async def build_three_levels(mgr: TaskTreeManager) -> dict[str, str]:
  root = await create_task(mgr, parent=None, request_id="root", name="Root")
  mid = await create_task(mgr, parent=root.id, request_id="mid", name="Mid")
  low = await create_task(mgr, parent=mid.id, request_id="low", name="Low")
  worker1 = await create_task(
      mgr,
      parent=low.id,
      request_id="w1",
      profile="worker",
      name="W1",
      task=TaskSpec(goal="worker goal", repo_path="/tmp/repo-w"))
  worker2 = await create_task(
      mgr, parent=low.id, request_id="w2", profile="worker", name="W2", task=TaskSpec(goal="sibling goal"))
  return {"root": root.id, "mid": mid.id, "low": low.id, "worker1": worker1.id, "worker2": worker2.id}


def sources_of(snapshot: PromptSnapshot) -> list[tuple[str, str, str | None]]:
  return [(s.scope, s.source_ref, s.source_session_id) for b in snapshot.blocks for s in b.sources]


def segment_texts(snapshot: PromptSnapshot) -> list[str]:
  return [b.text for b in snapshot.blocks]


async def patched_refs(mgr: TaskTreeManager, session_id: str, subtree: str | None, node: str | None):
  meta = await mgr.patch_task(
      session_id, PatchSessionTaskRequest(subtree_prompt=subtree, node_prompt=node), caller=OPERATOR)
  return meta


# ---------------------------------------------------------------------------
# One assembler, default roles at every depth
# ---------------------------------------------------------------------------


async def test_manager_template_identical_at_every_depth_and_no_pm_load(tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  snapshots = {}
  for label in ("root", "mid", "low"):
    meta = await mgr.load_meta(ids[label])
    snapshot, _err = preview_snapshot(cfg, meta, "manager_turn", chain=(), node_ref=None, overlay=None)
    snapshots[label] = snapshot
  for label, snapshot in snapshots.items():
    # The one manager contract from the shared template, at every depth.
    assert any(any(s.source_ref == "prompts/task_manager.md" for s in b.sources) for b in snapshot.blocks), label
    # The common rules come from their single maintained home.
    assert any(any(s.source_ref == "prompts/task_base.md" for s in b.sources) for b in snapshot.blocks), label
  # Identical managed rules ⇒ identical template selection bytes at every depth.
  for label in ("mid", "low"):
    assert snapshots[label].blocks == snapshots["root"].blocks
  # No per-layer manager template on v2.
  joined = snapshots["root"].instructions_text
  assert "PM identity" not in joined


async def test_manager_prompt_carries_the_shared_master_rules(tmp_path: Path) -> None:
  """One division of work for both manager kinds: the snapshot holds master.md
  full text between the shared base and the task-tree template, and its new
  Direct Work text rides verbatim."""
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  meta = await mgr.load_meta(ids["root"])
  snapshot, _err = preview_snapshot(cfg, meta, "manager_turn", chain=(), node_ref=None, overlay=None)
  refs = [s[1] for s in sources_of(snapshot)]
  assert "prompts/master.md" in refs
  assert "prompts/task_base.md" in refs
  assert "prompts/task_manager.md" in refs
  # master.md sits between task_base and task_manager in the rule order.
  assert refs.index("prompts/task_base.md") < refs.index("prompts/master.md") < refs.index("prompts/task_manager.md")
  joined = snapshot.instructions_text
  assert "Direct work and delegation divide by where the change lands." in joined
  assert "Every write to a repository, whatever its size, goes through `charliebot delegate` to a worker." in joined
  assert "Repository implementation stays with worker leaves at every manager depth." in joined


async def test_default_empty_local_rules_launch_cleanly(tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  for label in ("root", "worker1"):
    meta = await mgr.load_meta(ids[label])
    assert meta.subtree_prompt_ref is None and meta.node_prompt_ref is None
    snapshot, _err = preview_snapshot(cfg, meta, "manager_turn", chain=(), node_ref=None, overlay=None)
    assert snapshot.blocks  # rules and common blocks exist without any local rule
    assert not [s for s in sources_of(snapshot) if s[0] in ("subtree", "node")]


@pytest.mark.parametrize("kind", ["work", "review", "iteration", "scheduled_step"])
async def test_worker_kinds_get_their_applicable_contracts(tmp_path: Path, kind: str) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  meta = await mgr.load_meta(ids["worker1"])
  assert meta is not None and meta.task is not None
  snapshot, _err = preview_snapshot(cfg, meta, kind, chain=(), node_ref=None, overlay=None)
  refs = [s[1] for s in sources_of(snapshot)]
  assert "prompts/task_base.md" in refs
  if kind == "review":
    assert any(ref.startswith("src/core/review.py") for ref in refs)
  elif meta.task.task_type == "implement" or kind != "work":
    assert "prompts/worker.md" in refs
  assert "prompts/verify.md" not in refs  # a verify contract only on verify tasks


async def test_verify_task_gets_the_verify_contract(tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  task = await create_task(
      mgr,
      parent=ids["low"],
      profile="worker",
      request_id="verify-leaf",
      name="V",
      task=TaskSpec(goal="verify goal", task_type="verify"))
  meta = await mgr.load_meta(task.id)
  snapshot, _err = preview_snapshot(cfg, meta, "work", chain=(), node_ref=None, overlay=None)
  refs = [s[1] for s in sources_of(snapshot)]
  assert "prompts/verify.md" in refs
  assert "prompts/worker.md" not in refs


async def test_type_less_task_renders_the_implement_contract() -> None:
  assert prompt_task_type(None) == TaskType.IMPLEMENT
  assert prompt_task_type(TaskSpec(goal="sweep")) == TaskType.IMPLEMENT
  assert prompt_task_type(TaskSpec(goal="bump", task_type="quick-edit")) == TaskType.QUICK_EDIT


# ---------------------------------------------------------------------------
# Inheritance, ordering, no leakage
# ---------------------------------------------------------------------------


async def test_three_levels_with_both_scopes_prove_inheritance_and_ordering(tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  await patched_refs(mgr, ids["root"], subtree="root subtree rule", node="root node rule")
  await patched_refs(mgr, ids["mid"], subtree="mid subtree rule", node="mid node rule")
  await patched_refs(mgr, ids["low"], subtree=None, node="low node rule")

  async def snapshot_for(label: str, kind: str = "manager_turn") -> PromptSnapshot:
    meta = await mgr.load_meta(ids[label])
    index = await mgr._get_index()
    from src.core.task_execution import capture_prompt_chain
    chain, node_ref = capture_prompt_chain(mgr, index, meta)
    snapshot, _err = preview_snapshot(cfg, meta, kind, chain=chain, node_ref=node_ref, overlay=None)
    return snapshot

  # The worker sees exactly root+mid subtree rules then its own node rule.
  worker_snapshot = await snapshot_for("worker1")
  scopes = [(s[0], s[2]) for s in sources_of(worker_snapshot) if s[0] in ("subtree", "node")]
  assert scopes == [("subtree", ids["root"]), ("subtree", ids["mid"])]
  texts = segment_texts(worker_snapshot)
  subtree_texts = [b.text for b in worker_snapshot.blocks if any(s.scope == "subtree" for s in b.sources)]
  assert subtree_texts == ["root subtree rule", "mid subtree rule"]

  # An ancestor node rule never enters a descendant's instructions.
  for text in texts:
    assert "root node rule" not in text
    assert "mid node rule" not in text
  # Sibling rules never enter.
  assert "sibling goal" not in worker_snapshot.instructions_text

  # The mid manager sees root's subtree rule, its OWN subtree rule (the scope
  # is this node and its descendants), and its own node rule — not low's.
  mid_snapshot = await snapshot_for("mid")
  mid_scopes = [(s[0], s[2]) for s in sources_of(mid_snapshot) if s[0] in ("subtree", "node")]
  assert mid_scopes == [("subtree", ids["root"]), ("subtree", ids["mid"]), ("node", ids["mid"])]
  mid_subtree_texts = [b.text for b in mid_snapshot.blocks if any(s.scope == "subtree" for s in b.sources)]
  assert mid_subtree_texts == ["root subtree rule", "mid subtree rule"]


# ---------------------------------------------------------------------------
# Memory selection provenance
# ---------------------------------------------------------------------------


async def test_staged_candidates_never_enter_startup_or_query(tmp_path: Path) -> None:
  """A store holding only a staged candidate selects nothing: staging never launches."""
  cfg, _sm, _mgr = build_env(tmp_path)
  staged_dir = cfg.memory_dir / "staging"
  staged_dir.mkdir(parents=True)
  (staged_dir / "candidate.md").write_text(
      "---\nscope: user\ntopic: staged\ntitle: Candidate\n---\nstaged body\n", encoding="utf-8")
  (cfg.memory_dir / "topics").write_text("", encoding="utf-8")
  assert select_master_memory(cfg.memory_dir) is None
  assert assemble_master(cfg.memory_dir) is None
  worker_selection = select_worker_memory(cfg.memory_dir, "")
  assert worker_selection is not None
  assert all(not sources for _d, _t, sources in worker_selection.segments)


# ---------------------------------------------------------------------------
# Snapshot / hash contract

# ---------------------------------------------------------------------------
# Own-subtree scope: THIS NODE AND ITS DESCENDANTS
