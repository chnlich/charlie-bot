"""The one v2 task-context assembly owner: managed instruction blocks, snapshots, hashes.

Every v2 Run kind (manager turn, work, review, verify, improve iteration, cron
scheduled step) and the TUI terminal launch draw their managed instructions from
this module, and so does the preview API — one assembly path, never a
preview-only selector. The legacy (v1) master/worker assemblies in
``master_cc_run`` and ``spawner_prompt`` stay compatibility callers during the
migration; they never inject this module's output a second time.

Ordered managed instruction blocks (the contract the snapshot pins):

1. common execution rules (``prompts/task_base.md``) plus the run-kind / role /
   task-type rules (``prompts/task_manager.md`` for managers, the worker
   sections for worker kinds, the reviewer rules for review, the verify
   contract for verify) and the applicable host supplement
   (``cfg.claude_md_file``) and declared model overlay;
2. applicable memory, selected by :mod:`src.core.memory` (the single filter
   owner) — resident/repo full bodies, then the topic index;
3. each subtree rule from the root through this node (the subtree scope is
   THIS NODE AND ITS DESCENDANTS, so the node's own subtree rule applies to
   its own context too);
4. this node's own node rule.

An ancestor node rule and sibling rules never enter. Identical injected text is
emitted once, at its first ordered position, with every source merged there in
first-seen order; merely similar rules are never semantically merged. Task
goals, acceptance criteria, context refs, the actual input batch, prior
summaries, sequence/iteration bindings and worktree/branch facts are NOT managed
instructions: the adapters render them separately as task/input context, whose
exact launch text is retained as its own evidence (``launch_prompt.md``).

The snapshot object (``sessions/<id>/data/runs/<run_id>/prompt_snapshot.json``)
is ``{blocks, prompt_hash, char_count}`` exactly as plan 4.1 defines:
``blocks[i].sources`` lists every origin of that segment (``scope`` =
``base|memory|subtree|node``, ``source_ref`` = readable template/host/overlay/
memory/node origin, ``source_session_id`` = the owning node for local rules
only), ``body_ref`` is the SHA-256 of the segment's actual text, ``delivery``
distinguishes full text from an index, and ``text`` is what was injected.

``prompt_hash`` is the SHA-256 of the canonical JSON of the ordered blocks —
ordered instruction text plus source provenance — so it covers the selected
system/role/task-type rules, host/model supplements, memory and local rules,
and excludes run ids, timestamps, task progress and later input by
construction. Two launches over the same managed rules and sources produce the
same hash; changing an ancestor reference, a source identity, a template, the
applicable memory or a local rule changes it even when a transport-native
session id stays stable.

``char_count`` is the measured length of the joined instruction text actually
handed to the backend: ``"\\n\\n".join(block.text for block in blocks)`` — the
same join the adapters deliver, never an estimate or a token count.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

from src.core.config import CharlieBotConfig
from src.core.control_events import sha256_hex
from src.core.log_once import LazyStructlogLogger
from src.core.models import SessionMetadata, TaskType

if TYPE_CHECKING:
  from src.core.memory import MemorySelection

log = LazyStructlogLogger()

SNAPSHOT_FILENAME = "prompt_snapshot.json"
LAUNCH_TEXT_FILENAME = "launch_prompt.md"

SCOPE_BASE = "base"
SCOPE_MEMORY = "memory"
SCOPE_SUBTREE = "subtree"
SCOPE_NODE = "node"
DELIVERY_FULL = "full"

# Run kinds whose managed instructions are the manager contract (no worker
# workflow, no PM body, no project/PM rules — those retired with the v1 role).
MANAGER_KINDS = frozenset({"manager_turn"})
# Run kinds mapped to the worker audience for memory selection: workers,
# reviewers, verify, iteration and scheduled-step execution.
WORKER_KINDS = frozenset({"work", "review", "iteration", "scheduled_step"})

# Bounded rebuild loop for the launch-time coherence recheck: three passes
# absorb a burst of concurrent edits; a source that keeps moving fails the
# launch visibly instead of launching a snapshot nobody can reproduce.
_COHERENCE_PASSES = 3


class TaskPromptError(RuntimeError):
  """A managed instruction source is unavailable, malformed, or hash-invalid.

  The message names the source and the reason. It is a launch-blocking
  preparation failure (the queued Run and its unconsumed inputs stay
  untouched), never a silent skip or a degraded launch.
  """


@dataclass(frozen=True)
class PromptSource:
  """One managed block's origin: scope, readable ref, owning node (local rules only)."""
  scope: str  # base | memory | subtree | node
  source_ref: str
  source_session_id: str | None = None


@dataclass(frozen=True)
class PromptBlock:
  """One ordered managed instruction block with its full provenance."""
  sources: tuple[PromptSource, ...]
  body_ref: str
  delivery: str  # full | index
  text: str


@dataclass(frozen=True)
class PromptSnapshot:
  """The durable startup snapshot: ordered blocks, prompt hash, measured length."""
  blocks: tuple[PromptBlock, ...]

  @property
  def instructions_text(self) -> str:
    """The joined instruction text the backend actually receives (the measured join)."""
    return "\n\n".join(block.text for block in self.blocks)

  @property
  def char_count(self) -> int:
    return len(self.instructions_text)

  @property
  def prompt_hash(self) -> str:
    payload = json.dumps(
        {"blocks": [
            {"sources": [vars(s) for s in b.sources], "body_ref": b.body_ref,
             "delivery": b.delivery, "text": b.text}
            for b in self.blocks]},
        sort_keys=True, ensure_ascii=False)
    return sha256_hex(payload)

  def to_json_dict(self) -> dict:
    """The persisted/wire form: blocks, prompt_hash, char_count — the plan 4.1 object."""
    return {
        "blocks": [
            {
                "sources": [vars(s) for s in b.sources],
                "body_ref": b.body_ref,
                "delivery": b.delivery,
                "text": b.text,
            }
            for b in self.blocks
        ],
        "prompt_hash": self.prompt_hash,
        "char_count": self.char_count,
    }

  @classmethod
  def from_json_dict(cls, data: dict) -> PromptSnapshot:
    """Rebuild a stored snapshot; a malformed stored file raises (never silently serves)."""
    try:
      blocks = tuple(
          PromptBlock(
              sources=tuple(PromptSource(**s) for s in b["sources"]),
              body_ref=b["body_ref"],
              delivery=b["delivery"],
              text=b["text"],
          )
          for b in data["blocks"])
    except (KeyError, TypeError, ValueError) as e:
      raise TaskPromptError(f"malformed stored prompt snapshot: {e}") from e
    snapshot = cls(blocks=blocks)
    stored_hash = data.get("prompt_hash")
    stored_count = data.get("char_count")
    if stored_hash != snapshot.prompt_hash:
      raise TaskPromptError(
          f"stored prompt snapshot hash mismatch: recorded {stored_hash!r}, "
          f"content hashes to {snapshot.prompt_hash!r}")
    if stored_count != snapshot.char_count:
      raise TaskPromptError(
          f"stored prompt snapshot char_count mismatch: recorded {stored_count!r}, "
          f"content measures {snapshot.char_count}")
    for block in snapshot.blocks:
      if block.body_ref != sha256_hex(block.text):
        raise TaskPromptError(
            f"stored prompt snapshot body_ref mismatch: block hashes to "
            f"{sha256_hex(block.text)}, recorded {block.body_ref}")
    return snapshot


@dataclass(frozen=True)
class RuleSegment:
  """One ordered instruction unit before dedup: text, provenance, delivery label."""
  text: str
  sources: tuple[PromptSource, ...]
  delivery: str = DELIVERY_FULL


# ---------------------------------------------------------------------------
# Template readers (the owning sources; read fresh, never cached here)
# ---------------------------------------------------------------------------


def _read_source_file(path: Path, *, what: str) -> str:
  """Read one managed source file; missing/unreadable is a named TaskPromptError."""
  try:
    return path.read_text(encoding="utf-8")
  except (OSError, UnicodeDecodeError) as e:
    raise TaskPromptError(f"{what} is unavailable: {path} ({e})") from e


def _sections_text(cfg: CharlieBotConfig, filename: str, section_ids: tuple[str, ...]) -> str:
  """The joined marker-sections of one repo template (the section ids fail loud)."""
  from src.core.spawner_prompt import load_marker_sections
  path = cfg.charlie_bot_repo / "prompts" / filename
  sections = load_marker_sections(path, section_ids, extraction=f"{filename}-sections")
  return "\n".join(sections[sid] for sid in section_ids)


def _host_supplement(cfg: CharlieBotConfig, meta: SessionMetadata) -> str | None:
  """The host cfg.claude_md_file content (its YOUR_SESSION_UUID names this task node)."""
  path = cfg.claude_md_file
  if not path.exists():
    return None
  return _read_source_file(path, what="host supplement").replace("YOUR_SESSION_UUID", meta.id)


def _overlay_segment(cfg: CharlieBotConfig, overlay: str | None) -> tuple[str | None, OSError | None]:
  """The declared model overlay's text; a declared-but-unreadable file degrades like v1.

  The v1 contract (undeclared/unreadable overlay → the unified
  ``backend_overlay_inactive`` alert, run continues without the fence) is
  preserved: the caller emits the alert from the returned error. Any other
  overlay failure still propagates.
  """
  if overlay is None:
    return None, None
  path = cfg.charlie_bot_repo / "prompts" / "model_overlays" / f"{overlay}.md"
  try:
    return path.read_text(encoding="utf-8"), None
  except (OSError, UnicodeDecodeError) as e:
    return None, e


def _memory_rule_segments(selection: MemorySelection) -> list[RuleSegment]:
  """The memory selection's segments as ordered rule segments (scope=memory)."""
  segments: list[RuleSegment] = []
  for delivery, text, sources in selection.segments:
    segments.append(RuleSegment(
        text=text,
        sources=tuple(PromptSource(scope=SCOPE_MEMORY, source_ref=s.source_ref) for s in sources),
        delivery=delivery,
    ))
  return segments


def read_local_rule_body(prompt_bodies_dir: Path, ref: str, *, owner: str, scope: str) -> str:
  """Read one immutable local rule body and verify its fingerprint.

  ``ref`` names ``prompt_bodies/<sha256>.md``; the content must hash back to
  the ref. A missing or hash-invalid body is a named TaskPromptError — a
  corrupt or missing local rule never launches.
  """
  path = prompt_bodies_dir / f"{ref}.md"
  body = _read_source_file(path, what=f"{scope} rule of task {owner}")
  actual = sha256_hex(body)
  if actual != ref:
    raise TaskPromptError(
        f"{scope} rule of task {owner} is corrupt: prompt_bodies/{ref}.md hashes to {actual}")
  return body


# ---------------------------------------------------------------------------
# Per-kind rule segments
# ---------------------------------------------------------------------------


def _manager_rule_segments(cfg: CharlieBotConfig, meta: SessionMetadata) -> list[RuleSegment]:
  """The manager contract at any depth: common rules + the one manager template.

  Every manager depth selects this same template pair; project/feature
  differences live in the Task record and inherited rules. No PM identity, no
  project body, no per-layer template exists on v2.
  """
  base = _sections_text(cfg, "task_base.md", ("coding_principles", "skills_discovery", "remote_scratch"))
  contract = _sections_text(cfg, "task_manager.md", ("manager_role", "manager_boundaries"))
  segments = [
      RuleSegment(text=base, sources=(PromptSource(SCOPE_BASE, "prompts/task_base.md"),)),
      RuleSegment(text=contract, sources=(PromptSource(SCOPE_BASE, "prompts/task_manager.md"),)),
  ]
  host = _host_supplement(cfg, meta)
  if host is not None:
    segments.append(RuleSegment(text=host, sources=(PromptSource(SCOPE_BASE, str(cfg.claude_md_file)),)))
  return segments


def _worker_kind_rule_segments(
    cfg: CharlieBotConfig, meta: SessionMetadata, kind: str, task_type: TaskType
) -> list[RuleSegment]:
  """Worker-kind rules: role, the task-type workflow contract, and the source-files rule.

  The workflow's volatile bindings (branch/worktree/repo, the intro line) are
  NOT here — the adapters render them as task/input context from the same
  maintained sections.
  """
  base = _sections_text(cfg, "task_base.md", ("coding_principles", "skills_discovery", "remote_scratch"))
  segments = [RuleSegment(text=base, sources=(PromptSource(SCOPE_BASE, "prompts/task_base.md"),))]

  if kind == "review":
    from src.core.review import review_rules_text
    segments.append(RuleSegment(
        text=review_rules_text(),
        sources=(PromptSource(SCOPE_BASE, "src/core/review.py:review_rules_text"),)))
  elif task_type == TaskType.VERIFY:
    from src.core.verify_trailer import VERIFY_RESULT_TRAILER_EXPECTED
    contract = _sections_text(cfg, "verify.md", ("preamble", "scope"))
    from src.core.spawner_prompt import _substitute_tokens
    contract = _substitute_tokens(contract, {
        "{{result_trailer_expected}}": VERIFY_RESULT_TRAILER_EXPECTED,
        "{{canonical_template_path}}": str(
            (cfg.charlie_bot_repo / "prompts" / "plan_template.html").resolve()),
    })
    segments.append(RuleSegment(text=contract, sources=(PromptSource(SCOPE_BASE, "prompts/verify.md"),)))
  else:
    worker = _sections_text(cfg, "worker.md", ("role",))
    if task_type == TaskType.SCRIPT_RUN:
      workflow = _sections_text(cfg, "worker.md", ("workflow_script_run",))
    else:
      ending = "workflow_implement" if task_type == TaskType.IMPLEMENT else "workflow_quick_edit"
      workflow = _sections_text(cfg, "worker.md", ("workflow_steps", ending))
    source_files = _sections_text(cfg, "worker.md", ("task_spec_source_files",))
    segments.append(RuleSegment(
        text="\n".join((worker, workflow, source_files)),
        sources=(PromptSource(SCOPE_BASE, "prompts/worker.md"),)))
  return segments


def _overlay_rule_segments(
    cfg: CharlieBotConfig, overlay: str | None,
) -> tuple[list[RuleSegment], OSError | None]:
  segments, overlay_error = _overlay_segment(cfg, overlay)
  if segments is None:
    return [], overlay_error
  return ([RuleSegment(
      text=segments,
      sources=(PromptSource(SCOPE_BASE, f"prompts/model_overlays/{overlay}.md"),))], None)


def _local_rule_segments(
    prompt_bodies_dir: Path,
    chain: tuple[tuple[str, str | None], ...],
    node_ref: str | None,
    node_id: str,
) -> list[RuleSegment]:
  """The subtree rules (root → this node, inclusive) then this node's own rule.

  ``chain`` is the launch-time captured [(session_id, subtree_prompt_ref)] from
  the root down to and including this node (the subtree scope is this node and
  its descendants); ``node_ref`` is this node's own node rule. Ancestor node
  rules and sibling rules never enter. A null ref contributes nothing
  (default-empty local rules inherit cleanly).
  """
  segments: list[RuleSegment] = []
  for owner, ref in chain:
    if ref is None:
      continue
    segments.append(RuleSegment(
        text=read_local_rule_body(prompt_bodies_dir, ref, owner=owner, scope="subtree"),
        sources=(PromptSource(SCOPE_SUBTREE, f"prompt_bodies/{ref}.md", source_session_id=owner),)))
  if node_ref is not None:
    segments.append(RuleSegment(
        text=read_local_rule_body(prompt_bodies_dir, node_ref, owner=node_id, scope="node"),
        sources=(PromptSource(SCOPE_NODE, f"prompt_bodies/{node_ref}.md", source_session_id=node_id),)))
  return segments


# ---------------------------------------------------------------------------
# Assembly (dedup + snapshot)
# ---------------------------------------------------------------------------


def assemble_snapshot(segments: list[RuleSegment]) -> PromptSnapshot:
  """Dedup identical text at its first ordered position and build the snapshot.

  Every source of a repeated segment merges into the first block, in first-seen
  order; identical source tuples collapse, distinct references (the same body
  referenced by several ancestors) each stay listed. Merely similar rules are
  never merged — only byte-identical text dedups.
  """
  blocks: list[PromptBlock] = []
  by_text: dict[str, PromptBlock] = {}
  for segment in segments:
    existing = by_text.get(segment.text)
    if existing is not None:
      merged: list[PromptSource] = list(existing.sources)
      for source in segment.sources:
        if source not in merged:
          merged.append(source)
      replacement = PromptBlock(
          sources=tuple(merged), body_ref=existing.body_ref,
          delivery=existing.delivery, text=existing.text)
      blocks[blocks.index(existing)] = replacement
      by_text[segment.text] = replacement
      continue
    block = PromptBlock(
        sources=tuple(dict.fromkeys(segment.sources)),
        body_ref=sha256_hex(segment.text),
        delivery=segment.delivery,
        text=segment.text,
    )
    blocks.append(block)
    by_text[segment.text] = block
  return PromptSnapshot(blocks=tuple(blocks))


def memory_selection_for(meta: SessionMetadata, kind: str, cfg: CharlieBotConfig) -> MemorySelection | None:
  """The audience mapping: a manager maps to master; every worker-kind run to worker.

  The selection (filtering and formatting) stays owned by :mod:`src.core.memory`.
  Repo-less workers get the worker index only — never a guessed project.
  """
  # lazy: keeps the memory store off the M99 server import floor (docs/perf_baseline.md)
  from src.core.memory import select_master_memory, select_worker_memory

  if kind in MANAGER_KINDS:
    return select_master_memory(cfg.memory_dir)
  # A repo-less worker matches no repo topic: worker index only, never a guessed project.
  repo_basename = Path(meta.task.repo_path).name if (
      meta.task is not None and meta.task.repo_path) else ""
  return select_worker_memory(cfg.memory_dir, repo_basename)


def build_segments(
    cfg: CharlieBotConfig,
    meta: SessionMetadata,
    kind: str,
    *,
    overlay: str | None,
    chain: tuple[tuple[str, str | None], ...],
    node_ref: str | None,
) -> tuple[list[RuleSegment], OSError | None]:
  """One coherent assembly pass: rules, memory, local rules — in the contract's order."""
  if kind in MANAGER_KINDS:
    segments = _manager_rule_segments(cfg, meta)
  elif kind in WORKER_KINDS:
    task_type = meta.task.task_type if (meta.task is not None and meta.task.task_type) else TaskType.IMPLEMENT
    segments = _worker_kind_rule_segments(cfg, meta, kind, task_type)
  else:
    raise TaskPromptError(f"unknown run kind {kind!r}: no managed instruction contract")
  overlay_segments, overlay_error = _overlay_rule_segments(cfg, overlay)
  segments.extend(overlay_segments)
  selection = memory_selection_for(meta, kind, cfg)
  if selection is not None:
    segments.extend(_memory_rule_segments(selection))
  segments.extend(_local_rule_segments(cfg.charliebot_home / "prompt_bodies", chain, node_ref, meta.id))
  return segments, overlay_error


def preview_snapshot(
    cfg: CharlieBotConfig,
    meta: SessionMetadata,
    kind: str,
    *,
    chain: tuple[tuple[str, str | None], ...],
    node_ref: str | None,
    overlay: str | None,
) -> tuple[PromptSnapshot, OSError | None]:
  """The next-start snapshot-shaped preview: the same builder the launch path uses.

  A preview is the current configuration — never a separately assembled
  approximation. The returned overlay error (a declared-but-unreadable overlay)
  rides to the caller exactly as at launch.
  """
  segments, overlay_error = build_segments(cfg, meta, kind, overlay=overlay, chain=chain, node_ref=node_ref)
  return assemble_snapshot(segments), overlay_error


# ---------------------------------------------------------------------------
# Task/input context rendering (NOT part of the instruction hash)
#
# These render the volatile half of a launch — session identity, worktree /
# branch bindings, the pinned task spec and the actual input batch, sequence
# positions — from the same maintained template sections the managed rules use.
# The adapters persist this text as the launch's separate evidence
# (``launch_prompt.md``); it never enters the snapshot's instruction hash.
# ---------------------------------------------------------------------------


def render_session_info(cfg: CharlieBotConfig, session_name: str) -> str:
  from src.core.spawner_prompt import load_marker_sections
  sections = load_marker_sections(cfg.charlie_bot_repo / "prompts" / "worker.md", ("session_info",),
                                  extraction="worker-prompt")
  return sections["session_info"].replace("{{session_name}}", session_name)


def render_worktree_bindings(
    cfg: CharlieBotConfig,
    *,
    task_type: TaskType,
    intro_line: str,
    branch_name: str,
    base_branch_origin: str,
    wt_path: str,
    repo_path: str,
) -> str:
  """The workflow's binding header (intro + branch/worktree/repo), actual values."""
  from src.core.spawner_prompt import _substitute_tokens, load_marker_sections
  section = "workflow_script_run_bindings" if task_type == TaskType.SCRIPT_RUN else "worktree_bindings"
  sections = load_marker_sections(cfg.charlie_bot_repo / "prompts" / "worker.md", (section,),
                                  extraction="worker-prompt")
  return _substitute_tokens(sections[section], {
      "{{intro_line}}": intro_line,
      "{{branch_name}}": branch_name,
      "{{base_branch_origin}}": base_branch_origin,
      "{{wt_path}}": wt_path,
      "{{repo_path}}": repo_path,
  })


def render_task_body(cfg: CharlieBotConfig, description: str) -> str:
  from src.core.spawner_prompt import _substitute_tokens, load_marker_sections
  sections = load_marker_sections(cfg.charlie_bot_repo / "prompts" / "worker.md", ("task",),
                                  extraction="worker-prompt")
  return _substitute_tokens(sections["task"], {"{{description}}": description})


def render_iteration_reports(cfg: CharlieBotConfig, *, loop_dir: str, iteration_number: int) -> str:
  from src.core.spawner_prompt import _substitute_tokens, load_marker_sections
  sections = load_marker_sections(cfg.charlie_bot_repo / "prompts" / "worker.md", ("iteration_reports",),
                                  extraction="worker-prompt")
  return _substitute_tokens(sections["iteration_reports"], {
      "{{loop_dir}}": loop_dir,
      "{{iteration_number_padded}}": f"{iteration_number:04d}",
      "{{iteration_number}}": str(iteration_number),
  })


def render_worktree_persistence(cfg: CharlieBotConfig) -> str:
  from src.core.spawner_prompt import load_marker_sections
  sections = load_marker_sections(cfg.charlie_bot_repo / "prompts" / "worker.md", ("worktree_persistence",),
                                  extraction="worker-prompt")
  return sections["worktree_persistence"]


def review_task_context(
    *,
    branch_name: str,
    wt_path: str,
    base_branch: str,
    session_id: str,
    chat_log_path: Path,
    worker_log_path: Path,
    context_section: str,
) -> str:
  """The review run's volatile half: context, log paths, and this run's git steps.

  The same pieces build_review_prompt composes for v1 — one maintained source
  (src/core/review.py), two callers.
  """
  from src.core.review import review_numbered_steps
  return (
      f"## Context\n"
      f"{context_section}\n\n"
      f"If the summary above is insufficient or you are unsure about intent, "
      f"read the full logs: Session: `{chat_log_path}`, Worker: `{worker_log_path}`\n\n"
      f"The work is on branch `{branch_name}` in worktree `{wt_path}`. "
      f"All git operations below run from the worktree.\n\n"
      f"{review_numbered_steps(branch_name, wt_path, base_branch)}")
