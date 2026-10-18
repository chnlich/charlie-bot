"""Compare the thread contributions' instruction segments with the Rust contributions, answer by answer.

Each synthetic session is asked for its instruction segments by Python's web_reply, slack and discord turn
contributions, and by the Rust registries of the same packages (`examples/instruction_segments.rs`). The run kinds are
manager_turn, work and review, and each session runs against each synthetic checkout. Every segment's text, sources
and delivery must match byte for byte, and every failed build must carry the same error text.

One checkout lacks web_reply.md. There the web_reply build fails on both sides, and only that failure is compared:
Python raises FileNotFoundError ("[Errno 2] ..."), and Rust names the path it could not read.

Usage: uv run python rust/crates/features/chat_threads/tests/differential.py [path/to/instruction_segments]
"""

from __future__ import annotations

import dataclasses
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import types

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))

from src.app import registrations  # noqa: E402
from src.features.discord import turn_contribution as discord_turn  # noqa: E402
from src.features.slack import turn_contribution as slack_turn  # noqa: E402
from src.features.web_reply import turn_contribution as web_turn  # noqa: E402
from src.infra import models  # noqa: E402

DEFAULT_BINARY = ROOT / "rust/target/release/examples/instruction_segments"
KINDS = ("manager_turn", "work", "review")
KEYS = ("web_reply", "slack", "discord")
REAL_DOCS = (
    "web_reply.md",
    "slack_reply_scope.md",
    "discord_reply_scope.md",
    "thread_reply_redline.md",
    "thread_reply_format.md",
)
SLACK_ORIGIN = {"team_id": "synthetic-team", "channel_id": "synthetic-channel", "thread_ts": "1700000000.000100"}
DISCORD_ORIGIN = {"guild_id": "synthetic-guild", "parent_channel_id": "synthetic-parent", "thread_id": "1234567890"}


@dataclasses.dataclass(frozen=True)
class Session:
  """A synthetic session: its profile, its origin fields, and its parent."""

  id: str
  profile: str
  extra: dict
  parent: str | None = None


SESSIONS = (
    Session("synthetic-web", "manager", {}),
    Session("synthetic-slack-thread", "manager", {"slack_origin": SLACK_ORIGIN}),
    Session("synthetic-discord-thread", "manager", {"discord_origin": DISCORD_ORIGIN}),
    Session("synthetic-slack-worker", "worker", {"slack_origin": SLACK_ORIGIN}),
    Session("synthetic-discord-worker", "worker", {"discord_origin": DISCORD_ORIGIN}),
    Session("synthetic-null-origin", "manager", {
        "slack_origin": None,
        "discord_origin": None
    }),
    Session("synthetic-thread-child", "manager", {}, parent="synthetic-slack-thread"),
)


def write_checkout(root: pathlib.Path, texts: dict[str, str]) -> pathlib.Path:
  """A synthetic checkout whose `prompts/` holds `texts`, written byte for byte."""
  (root / "prompts").mkdir(parents=True)
  for name, text in texts.items():
    (root / "prompts" / name).write_bytes(text.encode("utf-8"))
  return root


def checkouts(scratch: pathlib.Path) -> dict[str, pathlib.Path]:
  """The synthetic checkouts: the real docs, the same docs with CRLF newlines and Python-only whitespace, and one
  checkout per doc that lacks that doc."""
  real = {name: (ROOT / "prompts" / name).read_bytes().decode("utf-8") for name in REAL_DOCS}
  padded = {name: "\u001c 　\r\n" + text.replace("\n", "\r\n") + "\r\n\t　\u001f" for name, text in real.items()}
  roots = {
      "real": write_checkout(scratch / "real", real),
      "padded": write_checkout(scratch / "padded", padded),
  }
  for name in REAL_DOCS:
    texts = {doc: text for doc, text in real.items() if doc != name}
    roots[f"without-{name}"] = write_checkout(scratch / f"without-{name}", texts)
  return roots


def segment_json(segment) -> dict:
  return {
      "text": segment.text,
      "sources":
          [
              {
                  "scope": source.scope,
                  "source_ref": source.source_ref,
                  "source_session_id": source.source_session_id,
              } for source in segment.sources
          ],
      "delivery": segment.delivery,
  }


def python_segments(contribution, meta: models.SessionMetadata, kind: str, cfg: types.SimpleNamespace) -> dict:
  try:
    segments = contribution.instruction_segments(meta, kind, cfg)
  except (OSError, ValueError) as error:
    return {"error": str(error)}
  return {"segments": [segment_json(segment) for segment in segments]}


def python_answer(session: Session, kind: str, repo: pathlib.Path) -> dict:
  meta = models.SessionMetadata(
      id=session.id, name=session.id, profile=session.profile, parent_session_id=session.parent, **session.extra)
  cfg = types.SimpleNamespace(charlie_bot_repo=repo)
  return {
      "web_reply": python_segments(web_turn.CONTRIBUTION, meta, kind, cfg),
      "slack": python_segments(slack_turn.CONTRIBUTION, meta, kind, cfg),
      "discord": python_segments(discord_turn.CONTRIBUTION, meta, kind, cfg),
  }


class RustContributions:
  """The Rust helper process: one JSON request per line in, one JSON answer per line out."""

  def __init__(self, binary: pathlib.Path) -> None:
    self._proc = subprocess.Popen(
        [str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8")

  @property
  def pid(self) -> int:
    return self._proc.pid

  def ask(self, request: dict) -> dict:
    self._proc.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
    self._proc.stdin.flush()
    line = self._proc.stdout.readline()
    if not line:
      raise RuntimeError(f"the Rust helper (pid {self._proc.pid}) closed its output")
    return json.loads(line)

  def close(self) -> None:
    self._proc.stdin.close()
    if self._proc.wait() != 0:
      raise RuntimeError(f"the Rust helper (pid {self._proc.pid}) exited with {self._proc.returncode}")


def rust_request(session: Session, kind: str, repo: pathlib.Path) -> dict:
  return {
      "repo": str(repo),
      "kind": kind,
      "meta":
          {
              "id": session.id,
              "name": session.id,
              "profile": session.profile,
              "parent_session_id": session.parent,
              "extra": session.extra,
          },
  }


def main(argv: list[str]) -> int:
  binary = pathlib.Path(argv[1]) if len(argv) > 1 else DEFAULT_BINARY
  registrations.register_all()
  segment_answers = dict.fromkeys(KEYS, 0)
  error_answers = dict.fromkeys(KEYS, 0)
  web_missing_failures = 0
  differences: list[str] = []
  cases = 0
  rust = RustContributions(binary)
  with tempfile.TemporaryDirectory(prefix="chat-threads-differential-") as scratch:
    roots = checkouts(pathlib.Path(scratch))
    try:
      for root_name, repo in roots.items():
        for session in SESSIONS:
          for kind in KINDS:
            cases += 1
            python = python_answer(session, kind, repo)
            rust_result = rust.ask(rust_request(session, kind, repo))
            for key in KEYS:
              where = f"{root_name} / {session.id} / {kind} / {key}"
              if "error" in python[key]:
                error_answers[key] += 1
              else:
                segment_answers[key] += bool(python[key]["segments"])
              if root_name == f"without-{REAL_DOCS[0]}" and key == "web_reply":
                if "error" in python[key] and "error" in rust_result[key]:
                  web_missing_failures += 1
                  continue
              if python[key] != rust_result[key]:
                differences.append(f"{where}:\n  python: {python[key]}\n  rust:   {rust_result[key]}")
    finally:
      rust.close()
  for difference in differences:
    print(difference)
  print(f"cases: {cases} (checkouts {len(roots)} x sessions {len(SESSIONS)} x kinds {len(KINDS)})")
  print(f"answers with segments: {segment_answers}")
  print(f"answers that fail the build: {error_answers}")
  print(f"web_reply missing-web_reply.md failures on both sides (texts not compared): {web_missing_failures}")
  print(f"differences: {len(differences)}")
  print(f"python pid: {os.getpid()}, rust pid: {rust.pid}")
  if not all(segment_answers.values()) or not all(error_answers.values()):
    print("the cases do not reach every contribution's segment and error paths")
    return 1
  return 1 if differences else 0


if __name__ == "__main__":
  sys.exit(main(sys.argv))
