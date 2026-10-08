"""Claude Code's usage log, read for the "Claude Code" usage source (src/runtime/hooks/usage_sources.py).

Each Claude login directory keeps its transcripts at ``<config_dir>/projects/**/*.jsonl``; every
assistant line carries ``message.usage`` and ``message.model``. The login directories are the
default ``~/.claude`` plus every ``accounts.claude`` entry of config.yaml, so a newly added pool
account joins the capture without an edit here.

Record id: ``claude:<message id, falling back to requestId, then uuid>``, one per response. Claude
Code replays history verbatim on resume and fork (about half of all usage lines on a busy host), so
a response dedupes on its id within a file here and across files and login directories in the
ledger's upsert. The record's session is the transcript's file name without its suffix.
"""

import os
import pathlib
from collections.abc import Iterator

from src.backends.claude_code import USAGE_SOURCE, login_dirs
from src.infra import config, ndjson
from src.infra import event_types as ET
from src.runtime.hooks import usage_sources

# A line carries usage only when its text holds this key, so the scan parses no other line.
_MARKERS = (b'"usage"',)


def _account_label(path: pathlib.Path) -> str:
  """The account label of a login directory: ``work (default)`` for ``.claude``, else the
  suffix after ``.claude-`` (``.claude-ext-1`` reads as ``ext-1``)."""
  if path.name == ".claude":
    return "work (default)"
  return path.name.removeprefix(".claude-")


def _login_dirs() -> dict[str, pathlib.Path]:
  """The login directories that hold transcripts, by account label."""
  dirs = {login_dirs.default_claude_dir()}
  for account in config.get_config().accounts.claude:
    dirs.add(pathlib.Path(account.config_dir).expanduser())
  return {_account_label(path): path for path in sorted(dirs) if (path / "projects").is_dir()}


def logs() -> Iterator[tuple[pathlib.Path, str]]:
  """Every transcript of every login directory, with its account label."""
  for label, login_dir in _login_dirs().items():
    for path in ndjson.iter_jsonl_files(str(login_dir / "projects")):
      yield pathlib.Path(path), label


def read(path: pathlib.Path, account: str, previous: str | None) -> tuple[str, list[usage_sources.UsageRecord]]:
  """One transcript's records and its ``<mtime_ns>:<size>`` signature, taken before the read.

  The whole file parses each time; the caller skips a file whose signature equals the stored one.
  """
  st = os.stat(path)
  records: list[usage_sources.UsageRecord] = []
  seen: set[str] = set()
  for line in ndjson.parse_marker_lines(path, _MARKERS):
    message = line.get("message")
    if not isinstance(message, dict):
      continue
    usage, model = message.get("usage"), message.get("model")
    if not isinstance(usage, dict) or not model or model == "<synthetic>":
      continue
    key = message.get("id") or line.get("requestId") or line.get("uuid")
    if key in seen:
      continue
    seen.add(key)
    in_fresh, cache_write, cache_read, output = ET.usage_counts(usage)
    records.append(
        usage_sources.UsageRecord(
            record_id=f"claude:{key}",
            kind=usage_sources.RecordKind.NATIVE,
            source=USAGE_SOURCE,
            model=model,
            account=account,
            ts=line.get("timestamp") or "",
            in_fresh=in_fresh,
            cache_write=cache_write,
            cache_read=cache_read,
            output=output,
            sessions=(path.stem,)))
  return f"{st.st_mtime_ns}:{st.st_size}", records


def quota_accounts() -> list[usage_sources.QuotaAccount]:
  """The Claude logins on the quota panel; the quota module loads on the first call."""
  from src.backends.claude_code import usage_quota

  return usage_quota.quota_accounts()


def sweep(scope: usage_sources.SweepScope) -> usage_sources.SourceSweep:
  """Claude Code's part of the cold-storage sweep; the sweep module loads on the first call."""
  from src.backends.claude_code import usage_sweep

  return usage_sweep.sweep(scope)
