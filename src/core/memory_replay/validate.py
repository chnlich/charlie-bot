"""Mechanical validation and finalization: from stage output to the reviewed patch.

Everything here is deterministic code, not model judgment: per-theme checks on
a stage's response (paths, entry formats, source refs, disposition coverage,
and the patch/disposition consistency of the stage's own final state), the
cross-theme conflict check and unified diff at final assembly, a round-trip
apply of that diff, and the changed-path-to-evidence mapping the proposal
schema requires. Any violation raises and the run fails visibly with nothing
written to any official surface.

Validation is also the source of the bounded repair loop's error list:
:func:`theme_output_errors` accumulates every violation of one stage's response
instead of stopping at the first, so the single re-ask can carry the complete
mechanical error list to the stage that made them.
"""

import re
from dataclasses import dataclass
from difflib import unified_diff
from pathlib import Path

from src.core import memory
from src.core.memory_replay.errors import ReplayValidationError
from src.core.memory_replay.exchange import ThemeOutput
from src.core.memory_replay.manifest import ENTRY_PATH_RE, Manifest, Theme

ACTIONS = ("new", "rewrite", "delete", "keep")
OUTCOMES = ("propose", "no_change", "needs_decision")


def canonical_text(text: str) -> str:
  """The newline normalization every diff side uses: one trailing newline, or empty."""
  stripped = text.rstrip("\n")
  return stripped + "\n" if stripped else ""


def theme_output_errors(
    output: ThemeOutput, *, role: str, manifest: Manifest, theme: Theme, allow_no_write_citations: bool) -> list[str]:
  """Every mechanical violation of one stage's response for one theme, in check order.

  Empty means the response is valid. ``allow_no_write_citations`` is the one
  contract difference between versions: the v3 contract permits optional
  evidence citations on ``keep``/``delete`` rows (validated against the
  available evidence); the v2 contract forbids them. Everything else — base-
  relative operations, the finite disposition-ref domain, path shapes, entry
  formats, coverage, and the stage's own patch/disposition consistency — is
  shared, because it is what the finalization mechanics require.
  """
  errors: list[str] = []
  base = {path: canonical_text(text) for path, text in manifest.base_entries().items()}
  entry_paths = manifest.theme_entry_paths(theme)
  entry_by_ref = {source.ref: source for source in entry_paths.values()}
  seen_paths: set[str] = set()
  for op in output.entries:
    if ENTRY_PATH_RE.match(op.path) is None:
      errors.append(f"{role}: entry path {op.path!r} is not entries/<topic>/<slug>.md")
      continue  # every later check for this operation keys on the path shape
    if op.path in seen_paths:
      errors.append(f"{role}: two entry operations target {op.path}")
    seen_paths.add(op.path)
    if op.action not in ACTIONS:
      errors.append(f"{role}: unknown entry action {op.action!r}")
      continue  # the remaining checks are action-specific
    if op.action in ("rewrite", "delete", "keep") and op.path not in entry_paths:
      errors.append(f"{role}: {op.action} targets {op.path}, which is not one of this theme's current entries")
    if op.action in ("delete", "keep"):
      if op.text is not None:
        errors.append(f"{role}: {op.action} on {op.path} must not carry text")
      if op.source_refs:
        if not allow_no_write_citations:
          errors.append(f"{role}: {op.action} on {op.path} must not carry source_refs")
        else:
          available = _available_ref_ids(manifest)
          for ref in op.source_refs:
            if ref not in available:
              errors.append(f"{role}: {op.action} on {op.path} cites unknown source ref {ref!r}")
    if not op.reason.strip():
      errors.append(f"{role}: {op.action} on {op.path} needs a non-empty reason")
    if op.action in ("new", "rewrite"):
      errors.extend(_entry_text_errors(op, role=role, manifest=manifest, existing=entry_paths.get(op.path)))
  covered, claimed_entries = _disposition_errors(
      output, role=role, manifest=manifest, theme=theme, errors=errors, entry_by_ref=entry_by_ref)
  missing = sorted(set(theme.candidate_refs) - covered)
  if missing:
    errors.append(f"{role}: no disposition row for candidate(s): {', '.join(missing)}")
  _remember_request_errors(output, role=role, manifest=manifest, theme=theme, covered=covered, errors=errors)
  errors.extend(_consistency_errors(output, role=role, base=base))
  return errors


def validate_theme_output(output: ThemeOutput, *, role: str, manifest: Manifest, theme: Theme) -> None:
  """Check one stage's response for one theme against the frozen inputs, v3 contract.

  Entry operations may only touch the theme's own current entries or add new
  ones in a declared topic; proposed texts must parse as format-v2 entries;
  source refs must resolve (optional citations on keep/delete included); every
  candidate of the theme needs exactly one disposition row; a change a stage
  initiates on an existing entry names that entry's source ref — the identity
  proposal.sources carries — never its store path; and the stage's own final
  state must agree with its propose rows, so patch/disposition inconsistencies
  surface at the stage that made them and can receive the bounded repair.
  """
  _raise_theme_errors(
      theme_output_errors(output, role=role, manifest=manifest, theme=theme, allow_no_write_citations=True))


def validate_theme_output_v2(output: ThemeOutput, *, role: str, manifest: Manifest, theme: Theme) -> None:
  """The v2 meanings, for interpreting recorded v2 runs only: keep/delete rows
  must not carry source_refs. Recorded v2 arms keep their original verdicts —
  a v2 response that v3 would accept stays failed here."""
  _raise_theme_errors(
      theme_output_errors(output, role=role, manifest=manifest, theme=theme, allow_no_write_citations=False))


def _raise_theme_errors(errors: list[str]) -> None:
  if errors:
    raise ReplayValidationError("\n".join(errors))


def _available_ref_ids(manifest: Manifest) -> set[str]:
  """Every ref a citation may resolve to: bundled evidence plus feedback provenance ids.

  A citation must resolve to the actually available evidence — a manifest
  source, or a feedback example's comment event / approved-change ref (the
  user's own edits are evidence too; proposal.json keeps feedback in its own
  field, so the id spaces stay separate).
  """
  ids = {f.comment_event for f in manifest.feedback_examples}
  ids |= {f.approved_change.approved_change_ref for f in manifest.feedback_examples if f.approved_change}
  return ids | {s.ref for s in manifest.sources}


def _entry_text_errors(op, *, role: str, manifest: Manifest, existing) -> list[str]:
  errors: list[str] = []
  if not op.text or not op.text.strip():
    errors.append(f"{role}: {op.action} on {op.path} needs the complete entry text")
  if op.action == "new" and op.path in manifest.base_paths:
    errors.append(f"{role}: new entry {op.path} already exists in the base")
  available = _available_ref_ids(manifest)
  for ref in op.source_refs:
    if ref not in available:
      errors.append(f"{role}: {op.action} on {op.path} cites unknown source ref {ref!r}")
  if not op.source_refs:
    errors.append(f"{role}: {op.action} on {op.path} cites no source refs as evidence")
  if op.action == "rewrite" and existing is not None and canonical_text(op.text or "") == canonical_text(existing.text):
    errors.append(f"{role}: rewrite of {op.path} is identical to the current entry (use keep, or change the text)")
  if not (op.text or "").strip():
    return errors
  try:
    entry = memory.parse_entry_text(op.text, entry_path=Path(op.path))
  except memory.MemoryFormatError as e:
    errors.append(f"{role}: proposed text for {op.path} is not a valid entry: {e}")
    return errors
  topics = {name: memory.Topic(name=name, resident=False) for name in manifest.topics}
  violations = memory.entry_violations(entry, topics)
  if violations:
    errors.append(f"{role}: proposed text for {op.path} violates the entry format: {violations[0]}")
  return errors


def _disposition_errors(
    output: ThemeOutput, *, role: str, manifest: Manifest, theme: Theme, errors: list[str],
    entry_by_ref: dict) -> tuple[set[str], set[str]]:
  """Check every disposition row; return (covered candidate refs, claimed entry refs)."""
  covered: set[str] = set()
  claimed_entries: set[str] = set()
  for row in output.candidates:
    if row.source_ref in theme.candidate_refs:
      if row.source_ref in covered:
        errors.append(f"{role}: two disposition rows for candidate {row.source_ref!r}")
      covered.add(row.source_ref)
    elif row.source_ref in entry_by_ref:
      # An entry-initiated change (editor maintenance or a reviewer reversal): the disposition
      # names the existing entry's source ref, so every changed path keeps its evidence mapping
      # even when no candidate asked for it; row.paths carries the store path separately.
      if row.outcome != "propose":
        errors.append(f"{role}: entry disposition for {row.source_ref!r} must be propose (it changes the entry)")
      if row.source_ref in claimed_entries:
        errors.append(f"{role}: two disposition rows for entry {row.source_ref!r}")
      claimed_entries.add(row.source_ref)
    else:
      hint = ""
      if row.source_ref.startswith("entries/"):
        hint = " (a store path is not a source_ref; use the entry's ref shown in evidence.entries)"
      elif row.source_ref.startswith("comment_event:") or row.source_ref in _available_ref_ids(manifest):
        hint = (
            " (a feedback id is evidence provenance, never a disposition source_ref; dispositions name "
            "candidate refs or current-entry refs only)")
      errors.append(
          f"{role}: disposition row names {row.source_ref!r}, which is neither a candidate of this theme nor "
          f"the ref of one of its current entries{hint}")
    if row.outcome not in OUTCOMES:
      errors.append(f"{role}: unknown outcome {row.outcome!r} for candidate {row.source_ref!r}")
    if not row.reason.strip():
      errors.append(f"{role}: candidate {row.source_ref!r} needs a non-empty reason")
    if row.outcome == "propose":
      if not row.paths:
        errors.append(f"{role}: propose row for {row.source_ref!r} lists no paths")
      for path in row.paths:
        if ENTRY_PATH_RE.match(path) is None:
          errors.append(f"{role}: propose row for {row.source_ref!r} lists malformed path {path!r}")
      if row.source_ref in claimed_entries and entry_by_ref[row.source_ref].path not in row.paths:
        errors.append(f"{role}: entry disposition for {row.source_ref!r} must list its own path")
    elif row.paths:
      errors.append(
          f"{role}: {row.outcome} row for {row.source_ref!r} must list no paths "
          f"(got {', '.join(row.paths)})")
  return covered, claimed_entries


def _remember_request_errors(
    output: ThemeOutput, *, role: str, manifest: Manifest, theme: Theme, covered: set[str], errors: list[str]) -> None:
  """An explicit remember request always keeps a visible disposition naming it."""
  by_ref = {row.source_ref: row for row in output.candidates}
  for ref in theme.candidate_refs:
    source = manifest.source(ref)
    if source is None or not source.remember_request or ref not in covered:
      continue  # uncovered refs already carry the coverage error
    row = by_ref[ref]
    if ref.lower() not in row.reason.lower() and "remember" not in row.reason.lower():
      errors.append(f"{role}: remember request {ref!r} has a disposition whose reason does not name it")


def _consistency_errors(output: ThemeOutput, *, role: str, base: dict[str, str]) -> list[str]:
  """The stage's own patch/disposition consistency, checked where it can be repaired.

  The stage's entry operations, applied to the base, must change exactly the
  paths its ``propose`` rows claim: a changed path with no propose row has no
  evidence mapping, and a propose row over an unchanged path points at nothing.
  Cross-theme conflicts (two themes driving the same path) cannot appear here —
  each theme sees only its own operations — and stay visible at final assembly
  in :func:`finalize`.
  """
  final = dict(base)
  for op in output.entries:
    if op.action == "delete":
      final.pop(op.path, None)
    elif op.action in ("rewrite", "new"):
      final[op.path] = canonical_text(op.text or "")
  changed = sorted(p for p in set(base) | set(final) if final.get(p) != base.get(p))
  propose_paths = {p for row in output.candidates if row.outcome == "propose" for p in row.paths}
  errors: list[str] = []
  unclaimed = [p for p in changed if p not in propose_paths]
  if unclaimed:
    errors.append(
        f"{role}: changed path(s) with no propose disposition mapping them to evidence: " + ", ".join(unclaimed))
  stale = sorted(propose_paths - set(changed))
  if stale:
    errors.append(f"{role}: propose disposition(s) listing path(s) the final diff does not change: " + ", ".join(stale))
  return errors


@dataclass
class FinalResult:
  final: dict[str, str]
  changed: list[str]
  patch: str


def finalize(base: dict[str, str], theme_outputs: list[tuple[Theme, ThemeOutput]]) -> FinalResult:
  """Apply every stage's entry operations, diff against the base, and prove the diff applies.

  Each stage's output has already passed :func:`validate_theme_output`, which
  checks its patch/disposition consistency; what only final assembly can see is
  a cross-theme conflict — two themes driving the same path — and it fails here,
  visibly. The reviewed patch is built over newline-normalized texts, then
  round-trip applied to prove it reconstructs the final state.
  """
  final = dict(base)
  targeted_by: dict[str, str] = {}
  for theme, output in sorted(theme_outputs, key=lambda pair: pair[0].name):
    for op in output.entries:
      if op.path in targeted_by and targeted_by[op.path] != theme.name:
        raise ReplayValidationError(
            f"cross-theme conflict: {theme.name} and {targeted_by[op.path]} both target {op.path}")
      targeted_by[op.path] = theme.name
      if op.action == "delete":
        final.pop(op.path, None)
      elif op.action in ("rewrite", "new"):
        final[op.path] = canonical_text(op.text or "")
  changed = sorted(p for p in set(base) | set(final) if final.get(p) != base.get(p))
  patch = build_patch(base, final)
  applied = apply_unified_patch(base, patch)
  for path in set(base) | set(final):
    if applied.get(path) != final.get(path):
      raise ReplayValidationError(f"generated patch does not round-trip for {path}")
  return FinalResult(final=final, changed=changed, patch=patch)


def build_patch(base: dict[str, str], final: dict[str, str]) -> str:
  """The unified diff of the final state against the base, store-relative paths, ``a/``/``b/`` form."""
  parts: list[str] = []
  for path in sorted(set(base) | set(final)):
    old, new = base.get(path), final.get(path)
    if old == new:
      continue
    parts.extend(
        unified_diff(
            (old or "").splitlines(), (new or "").splitlines(), fromfile=f"a/{path}", tofile=f"b/{path}", lineterm=""))
  return "\n".join(parts) + "\n" if parts else ""


def apply_unified_patch(base: dict[str, str], patch: str) -> dict[str, str | None]:
  """Apply a replay-generated patch to the base; raise when it does not apply.

  The result maps every touched path to its new text, and a deleted path to
  None. This is the mechanical proof that ``reviewed_patch`` is appliable to
  ``base_commit``'s content.

  The applicator is deliberately strict — no offset search, no context fuzz, no
  clamping: each hunk lands exactly where its header says, hunks come in order
  without overlap, and header counts must match the body. A patch that only a
  lenient applicator could place is a malformed patch, not a near miss. What
  the hunks leave around them — the unchanged prefix, the text between hunks,
  and the trailing suffix — survives into the result, so the applied state is
  the complete file, not just the span the hunks cover.
  """
  result: dict[str, str | None] = dict(base)
  lines = patch.split("\n")
  while lines and lines[-1] == "":
    lines.pop()
  i = 0
  seen_paths: set[str] = set()
  while i < len(lines):
    path, i = _read_file_header(lines, i)
    if path in seen_paths:
      raise ReplayValidationError(f"patch for {path}: the path appears in more than one file section")
    seen_paths.add(path)
    hunks: list[tuple[int, int, list[str]]] = []
    while i < len(lines) and lines[i].startswith("@@ "):
      old_start, old_count, body, i = _read_hunk(lines, i, path)
      hunks.append((old_start, old_count, body))
    if not hunks:
      raise ReplayValidationError(f"patch for {path}: no hunks")
    old_lines = (base.get(path) or "").splitlines()
    out: list[str] = []
    cursor = 0
    for old_start, old_count, body in hunks:
      cursor = _apply_hunk(out, old_lines, cursor, old_start, old_count, body, path)
    out.extend(old_lines[cursor:])  # the unchanged suffix after the last hunk survives
    result[path] = "\n".join(out) + "\n" if out else None
  return result


def _read_file_header(lines: list[str], i: int) -> tuple[str, int]:
  """Read one '--- a/<path>' / '+++ b/<path>' pair; the two must name the same path."""
  if not lines[i].startswith("--- a/"):
    raise ReplayValidationError(f"patch line {i + 1}: expected a '--- a/<path>' file header, got {lines[i]!r}")
  old_path = lines[i][len("--- a/"):]
  if not old_path:
    raise ReplayValidationError(f"patch line {i + 1}: file header has an empty path")
  i += 1
  if i >= len(lines) or not lines[i].startswith("+++ b/"):
    raise ReplayValidationError(f"patch line {i + 1}: expected a '+++ b/<path>' header after '--- a/{old_path}'")
  new_path = lines[i][len("+++ b/"):]
  if new_path != old_path:
    raise ReplayValidationError(f"patch: file headers disagree: '--- a/{old_path}' vs '+++ b/{new_path}'")
  return old_path, i + 1


def _read_hunk(lines: list[str], i: int, path: str) -> tuple[int, int, list[str], int]:
  """Read one hunk; return (1-based old-side start line, old-side count, body lines, next index)."""
  header = re.match(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@$", lines[i])
  if header is None:
    raise ReplayValidationError(f"patch for {path}: malformed hunk header {lines[i]!r}")
  old_start = int(header.group(1))
  old_count = int(header.group(2) if header.group(2) is not None else "1")
  new_count = int(header.group(4) if header.group(4) is not None else "1")
  if old_count == 0 and new_count == 0:
    raise ReplayValidationError(f"patch for {path}: hunk header {lines[i]!r} describes an empty hunk")
  i += 1
  body: list[str] = []
  seen_old = seen_new = 0
  while seen_old < old_count or seen_new < new_count:
    if i >= len(lines):
      raise ReplayValidationError(f"patch for {path}: hunk ends before its header counts are met")
    line = lines[i]
    prefix = line[:1]
    if prefix == " ":
      seen_old += 1
      seen_new += 1
    elif prefix == "-":
      seen_old += 1
    elif prefix == "+":
      seen_new += 1
    else:
      raise ReplayValidationError(f"patch for {path}: unexpected line in hunk: {line!r}")
    if seen_old > old_count or seen_new > new_count:
      raise ReplayValidationError(
          f"patch for {path}: hunk body disagrees with its header counts (old {seen_old}/{old_count}, new "
          f"{seen_new}/{new_count})")
    body.append(line)
    i += 1
  return old_start, old_count, body, i


def _apply_hunk(
    out: list[str], old_lines: list[str], cursor: int, old_start: int, old_count: int, body: list[str],
    path: str) -> int:
  """Copy the unchanged old lines before the hunk, then apply its body; return the new cursor.

  ``cursor`` counts the old lines already consumed; a hunk must start at or
  after it, so overlapping and out-of-order hunks fail here. A hunk that
  consumes old lines begins at 1-based ``old_start``; an empty old range
  (``-N,0``) inserts after line ``N``, the standard unified-diff convention.
  """
  anchor = old_start - 1 if old_count else old_start
  if anchor < cursor:
    raise ReplayValidationError(
        f"patch for {path}: hunk at old line {old_start} precedes or overlaps the previous hunk")
  if old_count and anchor + old_count > len(old_lines):
    raise ReplayValidationError(
        f"patch for {path}: hunk at old line {old_start} runs past the end of the file ({len(old_lines)} lines)")
  if not old_count and anchor > len(old_lines):
    raise ReplayValidationError(
        f"patch for {path}: hunk inserts after old line {old_start}, past the end of the file ({len(old_lines)} lines)")
  out.extend(old_lines[cursor:anchor])
  cursor = anchor
  for line in body:
    prefix, content = line[:1], line[1:]
    if prefix == " ":
      if cursor >= len(old_lines) or old_lines[cursor] != content:
        raise ReplayValidationError(f"patch for {path}: context line does not match the base: {content!r}")
      out.append(old_lines[cursor])
      cursor += 1
    elif prefix == "-":
      if cursor >= len(old_lines) or old_lines[cursor] != content:
        raise ReplayValidationError(f"patch for {path}: deleted line does not match the base: {content!r}")
      cursor += 1
    elif prefix == "+":
      out.append(content)
    else:
      raise ReplayValidationError(f"patch for {path}: unexpected hunk line {line!r}")
  return cursor
