"""Deterministic identity for replay inputs and proposals.

Two hashes, both over canonical JSON (sorted keys, no insignificant whitespace,
UTF-8), give replay its reuse rule and its approval binding:

- **input identity** covers everything the models can see plus everything that
  selects it: every source, the whole feedback pool, theme assignments, the
  mode, the model identity, the prompt versions, and — for the experimental
  variant contracts — the variant's behavioral definition. New relevant feedback,
  rule, or document evidence changes it, so a completed run is reused only
  while its inputs are still current. Derived from artifacts alone — there is
  no workflow state machine behind a rerun.
- **approval digest** covers base_commit and reviewed_patch only; it binds the
  page the user is shown to what they approve.
"""

import hashlib
import json

from src.core.memory_replay.manifest import Manifest


def canonical_bytes(payload: object) -> bytes:
  """The one fixed encoding every replay hash is computed over."""
  return json.dumps(payload, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def sha256_hex(data: bytes) -> str:
  return hashlib.sha256(data).hexdigest()


def input_identity(
    *,
    manifest: Manifest,
    mode: str,
    model_identity: dict,
    editor_prompt_version: str,
    reviewer_prompt_version: str,
    variant: dict | None = None,
) -> str:
  """The identity a completed run is reused on: frozen inputs + selection inputs + transport.

  ``variant`` carries the behavioral definition of an experimental variant contract (name,
  version, and its declared dimensions). It is absent — never an empty dict — for standalone
  v2/v3 runs, so their recorded identities stay byte-stable and archived runs are never
  silently reinterpreted under a variant key they were never run under.
  """
  payload = {
      "manifest_version": 1,
      "base_commit": manifest.base_commit,
      "topics": manifest.topics,
      "sources":
          [
              {
                  "ref": s.ref,
                  "kind": s.kind,
                  "path": s.path,
                  "sha256": s.sha256,
                  "remember_request": s.remember_request,
              } for s in manifest.sources
          ],
      "feedback_examples":
          [
              {
                  "comment_event":
                      f.comment_event,
                  "comment_text":
                      f.comment_text,
                  "tags":
                      f.tags,
                  "approved_change":
                      None if f.approved_change is None else {
                          "approved_change_ref": f.approved_change.approved_change_ref,
                          "before": f.approved_change.before,
                          "after": f.approved_change.after,
                      },
              } for f in manifest.feedback_examples
          ],
      "themes":
          [
              {
                  "name": t.name,
                  "principles": t.principles,
                  "candidate_refs": t.candidate_refs,
                  "entry_refs": t.entry_refs,
                  "document_refs": t.document_refs,
              } for t in manifest.themes
          ],
      "mode": mode,
      "model_identity": model_identity,
      "prompt_versions": {
          "editor": editor_prompt_version,
          "reviewer": reviewer_prompt_version,
      },
  }
  if variant is not None:
    payload["variant"] = variant
  return sha256_hex(canonical_bytes(payload))


def approval_digest(base_commit: str, reviewed_patch: str) -> str:
  """SHA-256 over the fixed encoding of base_commit and reviewed_patch jointly."""
  return sha256_hex(canonical_bytes({"base_commit": base_commit, "reviewed_patch": reviewed_patch}))
