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

import json
from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_ID, make_home_config

from src.core.control_events import sha256_hex
from src.core.memory import (
    assemble_master,
    assemble_worker,
    select_master_memory,
    select_worker_memory,
)
from src.core.models import PatchSessionTaskRequest, TaskSpec
from src.core.run_token import CallerIdentity
from src.core.sessions import SessionManager
from src.core.task_prompts import (
    PromptSnapshot,
    TaskPromptError,
    assemble_snapshot,
    build_segments,
    preview_snapshot,
)
from src.core.task_sessions import TaskTreeManager

pytestmark = pytest.mark.asyncio

OPERATOR = CallerIdentity(kind="operator")


def build_env(tmp_path: Path) -> tuple[object, SessionManager, TaskTreeManager]:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


async def create_task(mgr: TaskTreeManager, *, parent: str | None, profile: str = "manager",
                      request_id: str, name: str | None = None,
                      task: TaskSpec | None = None, **kwargs: object):
  return await mgr.create_task(
      request_id=request_id, task_parent_id=parent, profile=profile, task=task,
      name=name, backend=OPUS_BACKEND_ID, caller=OPERATOR, **kwargs)


async def build_three_levels(mgr: TaskTreeManager) -> dict[str, str]:
  root = await create_task(mgr, parent=None, request_id="root", name="Root")
  mid = await create_task(mgr, parent=root.id, request_id="mid", name="Mid")
  low = await create_task(mgr, parent=mid.id, request_id="low", name="Low")
  worker1 = await create_task(
      mgr, parent=low.id, request_id="w1", profile="worker", name="W1",
      task=TaskSpec(goal="worker goal", repo_path="/tmp/repo-w"))
  worker2 = await create_task(
      mgr, parent=low.id, request_id="w2", profile="worker", name="W2",
      task=TaskSpec(goal="sibling goal"))
  return {"root": root.id, "mid": mid.id, "low": low.id,
          "worker1": worker1.id, "worker2": worker2.id}


def sources_of(snapshot: PromptSnapshot) -> list[tuple[str, str, str | None]]:
  return [(s.scope, s.source_ref, s.source_session_id)
          for b in snapshot.blocks for s in b.sources]


def segment_texts(snapshot: PromptSnapshot) -> list[str]:
  return [b.text for b in snapshot.blocks]


async def patched_refs(mgr: TaskTreeManager, session_id: str, subtree: str | None, node: str | None):
  meta = await mgr.patch_task(
      session_id,
      PatchSessionTaskRequest(subtree_prompt=subtree, node_prompt=node),
      caller=OPERATOR)
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
    assert any(
        any(s.source_ref == "prompts/task_manager.md" for s in b.sources)
        for b in snapshot.blocks), label
    # The common rules come from their single maintained home.
    assert any(
        any(s.source_ref == "prompts/task_base.md" for s in b.sources)
        for b in snapshot.blocks), label
  # Identical managed rules ⇒ identical template selection bytes at every depth.
  for label in ("mid", "low"):
    assert snapshots[label].blocks == snapshots["root"].blocks
  # No PM identity, no project body, no per-layer manager template on v2.
  joined = snapshots["root"].instructions_text
  assert "project_manager" not in joined
  assert "Project Manager" not in joined
  assert "PM identity" not in joined


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
      mgr, parent=ids["low"], profile="worker", request_id="verify-leaf", name="V",
      task=TaskSpec(goal="verify goal", task_type="verify"))
  meta = await mgr.load_meta(task.id)
  snapshot, _err = preview_snapshot(cfg, meta, "work", chain=(), node_ref=None, overlay=None)
  refs = [s[1] for s in sources_of(snapshot)]
  assert "prompts/verify.md" in refs
  assert "prompts/worker.md" not in refs


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
    snapshot, _err = preview_snapshot(
        cfg, meta, kind, chain=chain, node_ref=node_ref, overlay=None)
    return snapshot

  # The worker sees exactly root+mid subtree rules then its own node rule.
  worker_snapshot = await snapshot_for("worker1")
  scopes = [(s[0], s[2]) for s in sources_of(worker_snapshot) if s[0] in ("subtree", "node")]
  assert scopes == [("subtree", ids["root"]), ("subtree", ids["mid"])]
  texts = segment_texts(worker_snapshot)
  subtree_texts = [b.text for b in worker_snapshot.blocks
                   if any(s.scope == "subtree" for s in b.sources)]
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
  assert mid_scopes == [
      ("subtree", ids["root"]), ("subtree", ids["mid"]), ("node", ids["mid"])]
  mid_subtree_texts = [b.text for b in mid_snapshot.blocks
                       if any(s.scope == "subtree" for s in b.sources)]
  assert mid_subtree_texts == ["root subtree rule", "mid subtree rule"]


async def test_exact_body_dedup_merges_every_source_at_first_position(tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  same_rule = "the shared delivery rule"
  await patched_refs(mgr, ids["root"], subtree=same_rule, node=None)
  await patched_refs(mgr, ids["mid"], subtree=same_rule, node=None)

  meta = await mgr.load_meta(ids["low"])
  index = await mgr._get_index()
  from src.core.task_execution import capture_prompt_chain
  chain, node_ref = capture_prompt_chain(mgr, index, meta)
  snapshot, _err = preview_snapshot(cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)

  # Identical injected text emitted once, at the first ordered position, with
  # every source merged there in first-seen order.
  matching = [b for b in snapshot.blocks if b.text == same_rule]
  assert len(matching) == 1
  assert [(s.source_session_id) for s in matching[0].sources] == [ids["root"], ids["mid"]]
  # The block carries the first ordered position: before later subtree rules.
  position = snapshot.blocks.index(matching[0])
  later = [b for b in snapshot.blocks[position + 1:]]
  assert all(b.text != same_rule for b in later)
  # Merely similar rules are never merged: the low node's own rule differs.
  await patched_refs(mgr, ids["low"], subtree=None, node=same_rule + " (variant)")
  meta = await mgr.load_meta(ids["low"])
  index = await mgr._get_index()
  chain, node_ref = capture_prompt_chain(mgr, index, meta)
  variant_snapshot, _err = preview_snapshot(
      cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
  assert len([b for b in variant_snapshot.blocks if b.text.startswith(same_rule)]) == 2


async def test_task_goal_and_context_refs_are_not_copied_into_persistent_rules(
        tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  meta = await mgr.load_meta(ids["worker1"])
  assert meta is not None
  # The goal lives in the Task record; the rule refs stay untouched.
  assert meta.task is not None and meta.task.goal == "worker goal"
  assert meta.subtree_prompt_ref is None and meta.node_prompt_ref is None
  goal_text = meta.task.goal
  snapshot, _err = preview_snapshot(
      cfg, meta, "manager_turn", chain=((ids["root"], None),), node_ref=None, overlay=None)
  # Nothing about the goal is manufactured into any node rule.
  patched = await mgr.patch_task(
      ids["worker1"], PatchSessionTaskRequest(name="renamed only"), caller=OPERATOR)
  assert patched.node_prompt_ref is None and patched.subtree_prompt_ref is None
  assert goal_text == "worker goal"


# ---------------------------------------------------------------------------
# Memory selection provenance
# ---------------------------------------------------------------------------


def write_memory_fixture(cfg, entries: list[tuple[str, str, str, str, str]], topics: str) -> Path:
  """(topic, slug, audience, title, body) entries; topics is the topics file content."""
  memory_dir = cfg.memory_dir
  for topic, slug, audience, title, body in entries:
    entry_dir = memory_dir / "entries" / topic
    entry_dir.mkdir(parents=True, exist_ok=True)
    front = f"---\nscope: user\ntopic: {topic}\naudience: {audience}\ntitle: {title}\n---\n"
    (entry_dir / f"{slug}.md").write_text(front + body, encoding="utf-8")
  (memory_dir / "topics").write_text(topics, encoding="utf-8")
  return memory_dir


@pytest.mark.parametrize(
    ("entries", "topics", "audience", "repo", "expect_full", "expect_index"),
    [
        # master-only, non-resident topic: the master indexes it.
        ([("alpha", "a1", "master", "Master Only", "m-body")], "alpha\n",
         "master", "nomatch", False, True),
        # master-only entry, worker audience: not worker-visible at all — the
        # worker's repo-topic full bodies are gated by the audience too.
        ([("alpha", "a1", "master", "Master Only", "m-body")], "alpha\n",
         "worker", "alpha", False, False),
        # worker-only repo topic: full for the matching repo.
        ([("proj", "p1", "worker", "Proj Entry", "w-body")], "proj\n",
         "worker", "proj", True, False),
        # ...and index-only for a non-matching repo.
        ([("proj", "p1", "worker", "Proj Entry", "w-body")], "proj\n",
         "worker", "other", False, True),
        # both audiences on a resident topic: the master gets the full body.
        ([("beta", "b1", "master, worker", "Both", "b-body")], "beta resident\n",
         "master", "nomatch", True, False),
    ])
async def test_memory_selection_provenance_matches_legacy_strings(
        tmp_path: Path, entries, topics, audience, repo, expect_full, expect_index) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  write_memory_fixture(cfg, entries, topics)
  staged_dir = cfg.memory_dir / "staging"
  staged_dir.mkdir(parents=True, exist_ok=True)
  (staged_dir / "candidate.md").write_text(
      "---\nscope: user\ntopic: staged\ntitle: Candidate\n---\nstaged body\n", encoding="utf-8")
  selection = (select_master_memory(cfg.memory_dir) if audience == "master"
               else select_worker_memory(cfg.memory_dir, repo))
  legacy = (assemble_master(cfg.memory_dir) if audience == "master"
            else assemble_worker(cfg.memory_dir, repo))
  assert selection is not None
  assert selection.text == legacy  # the legacy string derives from the same selection
  # Entry-bearing segments (the worker usage line rides an index segment with
  # no sources of its own).
  full_with_entries = any(
      d == "full" and sources for d, _t, sources in selection.segments)
  index_with_entries = any(
      d == "index" and sources for d, _t, sources in selection.segments)
  assert full_with_entries == expect_full
  assert index_with_entries == expect_index
  for _d, _text, sources in selection.segments:
    for source in sources:
      assert source.source_ref.startswith("memory:")
  # The assembly injects the same filtered bytes exactly once.
  meta = await mgr.create_task(
      request_id="m", task_parent_id=None,
      profile="manager" if audience == "master" else "worker",
      task=TaskSpec(goal="g", repo_path=f"/tmp/{repo}" if repo != "nomatch" else None),
      name="M", backend=OPUS_BACKEND_ID, caller=OPERATOR)
  fresh = await mgr.load_meta(meta.id)
  assert fresh is not None
  kind = "manager_turn" if audience == "master" else "work"
  segments, _err = build_segments(cfg, fresh, kind, overlay=None, chain=(), node_ref=None)
  memory_blocks = [s for s in segments if s.sources and s.sources[0].scope == "memory"]
  if expect_full or expect_index:
    assert memory_blocks
    for text in (s.text for s in memory_blocks):
      assert text in (legacy or "")  # every injected memory chunk is the legacy bytes
    whole = "\n\n".join(s.text for s in segments)
    assert whole.count(memory_blocks[0].text) == 1
  else:
    # Not visible to this audience: no memory block carries any entry source.
    assert not memory_blocks


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


async def test_repo_less_worker_gets_worker_index_only(tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  write_memory_fixture(
      cfg, [("proj", "p1", "worker", "Proj", "w-body"), ("shared", "s1", "worker", "Shared", "x-body")],
      "proj\nshared\n")
  task = await create_task(mgr, parent=None, profile="worker", request_id="w", name="W",
                           task=TaskSpec(goal="g"))
  meta = await mgr.load_meta(task.id)
  segments, _err = build_segments(cfg, meta, "work", overlay=None, chain=(), node_ref=None)
  memory_segments = [s for s in segments if s.sources and s.sources[0].scope == "memory"]
  joined = "\n\n".join(s.text for s in memory_segments)
  assert "shared/s1" in joined          # index line for the non-repo topic
  assert "proj/p1" in joined            # repo-matching topic also indexes (repo-less)
  assert "w-body" not in joined         # no guessed project: no full bodies


# ---------------------------------------------------------------------------
# Snapshot / hash contract
# ---------------------------------------------------------------------------


async def test_snapshot_hash_and_char_count_semantics(tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  await patched_refs(mgr, ids["root"], subtree="root subtree rule", node=None)

  async def assembled():
    meta = await mgr.load_meta(ids["low"])
    index = await mgr._get_index()
    from src.core.task_execution import capture_prompt_chain
    chain, node_ref = capture_prompt_chain(mgr, index, meta)
    snapshot, _err = preview_snapshot(
        cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
    return snapshot

  base_snapshot = await assembled()
  assert base_snapshot.char_count == len(base_snapshot.instructions_text)
  assert base_snapshot.char_count == sum(
      len(b.text) for b in base_snapshot.blocks) + 2 * (len(base_snapshot.blocks) - 1)
  # Body refs are SHA-256 of the actual text.
  for block in base_snapshot.blocks:
    assert block.body_ref == sha256_hex(block.text)

  # Same managed rules and sources ⇒ same hash.
  again = await assembled()
  assert again.prompt_hash == base_snapshot.prompt_hash

  # Changing a local rule changes it (the node's own rule enters its assembly).
  await patched_refs(mgr, ids["low"], subtree=None, node="a changed low node rule")
  assert (await assembled()).prompt_hash != base_snapshot.prompt_hash

  # Changing the template changes it.
  manager_md = cfg.charlie_bot_repo / "prompts" / "task_manager.md"
  original = manager_md.read_text(encoding="utf-8")
  try:
    manager_md.write_text(
        original.replace("You are the manager of one task node.",
                         "You are the manager of one task node (edited)."),
        encoding="utf-8")
    assert (await assembled()).prompt_hash not in (base_snapshot.prompt_hash,)
  finally:
    manager_md.write_text(original, encoding="utf-8")

  # Changing applicable memory changes it.
  write_memory_fixture(cfg, [("alpha", "a1", "master", "Alpha", "alpha body")], "alpha resident\n")
  memory_snapshot = await assembled()
  assert memory_snapshot.prompt_hash not in (base_snapshot.prompt_hash,)


async def test_snapshot_round_trip_and_tamper_detection(tmp_path: Path) -> None:
  from src.core.task_prompts import PromptSource, RuleSegment
  snapshot = assemble_snapshot([
      RuleSegment(text="rule one", sources=(PromptSource("base", "prompts/task_base.md"),)),
      RuleSegment(text="rule two", sources=(PromptSource("node", "prompt_bodies/x.md", "sid"),)),
  ])
  data = snapshot.to_json_dict()
  assert set(data) == {"blocks", "prompt_hash", "char_count"}
  restored = PromptSnapshot.from_json_dict(json.loads(json.dumps(data)))
  assert restored.prompt_hash == snapshot.prompt_hash
  assert restored.instructions_text == snapshot.instructions_text
  # A tampered text is refused (hash-invalid), never silently served.
  tampered = json.loads(json.dumps(data))
  tampered["blocks"][0]["text"] = "rule one TAMPERED"
  with pytest.raises(TaskPromptError):
    PromptSnapshot.from_json_dict(tampered)
  tampered = json.loads(json.dumps(data))
  tampered["char_count"] = 1
  with pytest.raises(TaskPromptError):
    PromptSnapshot.from_json_dict(tampered)


async def test_corrupt_local_rule_fails_loudly(tmp_path: Path) -> None:
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  meta = await patched_refs(mgr, ids["low"], subtree=None, node="rule body")
  assert meta.node_prompt_ref is not None
  # Corrupt the stored body: reads must fail with the reason, never launch.
  body_path = cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md"
  body_path.write_text("tampered body", encoding="utf-8")
  from src.core.task_execution import capture_prompt_chain
  index = await mgr._get_index()
  chain, node_ref = capture_prompt_chain(mgr, index, meta)
  with pytest.raises(TaskPromptError, match="corrupt"):
    preview_snapshot(cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
  # A missing body fails with its reason too.
  body_path.unlink()
  with pytest.raises(TaskPromptError, match="unavailable"):
    preview_snapshot(cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)


# ---------------------------------------------------------------------------
# Own-subtree scope: THIS NODE AND ITS DESCENDANTS
# ---------------------------------------------------------------------------


async def test_own_subtree_rule_applies_to_self(tmp_path: Path) -> None:
  """A node's own subtree rule enters its own next context, before its node rule.

  Regression: the launch chain collected ancestor subtree refs only and dropped
  the current node's own subtree rule, so editing "this task and descendants"
  changed every descendant's context but never the node's own — the approved
  scope is THIS NODE AND ITS DESCENDANTS.
  """
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  await patched_refs(mgr, ids["mid"], subtree="mid shared rule", node="mid local rule")

  async def snapshot_for(label: str) -> PromptSnapshot:
    meta = await mgr.load_meta(ids[label])
    index = await mgr._get_index()
    from src.core.task_execution import capture_prompt_chain
    chain, node_ref = capture_prompt_chain(mgr, index, meta)
    snapshot, _err = preview_snapshot(
        cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
    return snapshot

  mid_snapshot = await snapshot_for("mid")
  scopes = [(s[0], s[2]) for s in sources_of(mid_snapshot) if s[0] in ("subtree", "node")]
  # Own subtree rule, then own node rule: the manager's own context carries both.
  assert scopes == [("subtree", ids["mid"]), ("node", ids["mid"])]
  texts = [b.text for b in mid_snapshot.blocks if any(s.scope == "subtree" for s in b.sources)]
  assert texts == ["mid shared rule"]

  # A descendant still inherits it through the ancestor chain.
  worker_meta = await mgr.load_meta(ids["worker1"])
  index = await mgr._get_index()
  from src.core.task_execution import capture_prompt_chain
  chain, node_ref = capture_prompt_chain(mgr, index, worker_meta)
  worker_snapshot, _err = preview_snapshot(
      cfg, worker_meta, "work", chain=chain, node_ref=node_ref, overlay=None)
  worker_scopes = [(s[0], s[2]) for s in sources_of(worker_snapshot) if s[0] in ("subtree", "node")]
  assert worker_scopes == [("subtree", ids["mid"])]


async def test_root_own_subtree_rule_applies_with_no_ancestors(tmp_path: Path) -> None:
  """A root's own subtree rule applies to the root itself (no ancestors needed)."""
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  await patched_refs(mgr, ids["root"], subtree="root program rule", node=None)

  meta = await mgr.load_meta(ids["root"])
  index = await mgr._get_index()
  from src.core.task_execution import capture_prompt_chain
  chain, node_ref = capture_prompt_chain(mgr, index, meta)
  assert chain == ((ids["root"], meta.subtree_prompt_ref),) and node_ref is None
  snapshot, _err = preview_snapshot(
      cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
  scopes = [(s[0], s[2]) for s in sources_of(snapshot) if s[0] in ("subtree", "node")]
  assert scopes == [("subtree", ids["root"])]
  assert [b.text for b in snapshot.blocks if any(s.scope == "subtree" for s in b.sources)] == [
      "root program rule"]


async def test_middle_manager_with_both_scopes_orders_subtree_then_node(tmp_path: Path) -> None:
  """Both scopes on one node: inherited rule, own subtree rule, own node rule."""
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  await patched_refs(mgr, ids["root"], subtree="root shared rule", node=None)
  await patched_refs(mgr, ids["mid"], subtree="mid shared rule", node="mid local rule")

  meta = await mgr.load_meta(ids["mid"])
  index = await mgr._get_index()
  from src.core.task_execution import capture_prompt_chain
  chain, node_ref = capture_prompt_chain(mgr, index, meta)
  snapshot, _err = preview_snapshot(
      cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
  scopes = [(s[0], s[2]) for s in sources_of(snapshot) if s[0] in ("subtree", "node")]
  assert scopes == [
      ("subtree", ids["root"]), ("subtree", ids["mid"]), ("node", ids["mid"])]
  subtree_texts = [b.text for b in snapshot.blocks if any(s.scope == "subtree" for s in b.sources)]
  assert subtree_texts == ["root shared rule", "mid shared rule"]

  # A worker two levels below sees both subtree rules and never the mid node rule.
  worker_meta = await mgr.load_meta(ids["worker1"])
  chain, node_ref = capture_prompt_chain(mgr, index, worker_meta)
  worker_snapshot, _err = preview_snapshot(
      cfg, worker_meta, "work", chain=chain, node_ref=node_ref, overlay=None)
  worker_scopes = [(s[0], s[2]) for s in sources_of(worker_snapshot) if s[0] in ("subtree", "node")]
  assert worker_scopes == [("subtree", ids["root"]), ("subtree", ids["mid"])]
  assert "mid local rule" not in worker_snapshot.instructions_text


async def test_worker_own_subtree_rule_applies_to_itself(tmp_path: Path) -> None:
  """A worker's own subtree rule (a leaf's rule also applies to itself)."""
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  await patched_refs(mgr, ids["worker1"], subtree="worker shared rule", node=None)
  meta = await mgr.load_meta(ids["worker1"])
  index = await mgr._get_index()
  from src.core.task_execution import capture_prompt_chain
  chain, node_ref = capture_prompt_chain(mgr, index, meta)
  snapshot, _err = preview_snapshot(cfg, meta, "work", chain=chain, node_ref=node_ref, overlay=None)
  scopes = [(s[0], s[2]) for s in sources_of(snapshot) if s[0] in ("subtree", "node")]
  assert scopes == [("subtree", ids["worker1"])]

  # Siblings stay isolated: worker2's context never sees worker1's rule.
  sibling_meta = await mgr.load_meta(ids["worker2"])
  chain, node_ref = capture_prompt_chain(mgr, index, sibling_meta)
  sibling_snapshot, _err = preview_snapshot(
      cfg, sibling_meta, "work", chain=chain, node_ref=node_ref, overlay=None)
  assert "worker shared rule" not in sibling_snapshot.instructions_text


async def test_equal_body_dedup_merges_own_subtree_with_ancestors(tmp_path: Path) -> None:
  """Identical text on an ancestor subtree and the node's own subtree dedups
  once at the first position with every source listed."""
  cfg, _sm, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  same_rule = "deliver with evidence"
  await patched_refs(mgr, ids["root"], subtree=same_rule, node=None)
  await patched_refs(mgr, ids["mid"], subtree=same_rule, node=None)
  await patched_refs(mgr, ids["low"], subtree=same_rule, node=None)

  meta = await mgr.load_meta(ids["low"])
  index = await mgr._get_index()
  from src.core.task_execution import capture_prompt_chain
  chain, node_ref = capture_prompt_chain(mgr, index, meta)
  snapshot, _err = preview_snapshot(
      cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
  matching = [b for b in snapshot.blocks if b.text == same_rule]
  assert len(matching) == 1
  assert [s.source_session_id for s in matching[0].sources] == [ids["root"], ids["mid"], ids["low"]]
