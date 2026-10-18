"""Python oracle for the codex package's Rust port (``charliebot-agents::backends::codex``).

Reads one JSON request on stdin and writes one JSON document as the last line of stdout. The request names a synthetic
home and a PATH that holds a fake codex binary; HOME and PATH point there before the package is imported, so the
default codex home and the binary resolve inside it. It starts no agent CLI and makes no network call.
"""

import asyncio
import dataclasses
import datetime
import json
import os
import pathlib
import sys
from types import SimpleNamespace


def _main() -> None:
  request = json.load(sys.stdin)
  os.environ["HOME"] = request["home"]
  os.environ["PATH"] = request["path"]

  from src.app import registrations

  registrations.register_all()
  result = {
      "argv": _argv_section(request),
      "env": _env_section(request),
      "translate": _translate_section(request),
      "read": _read_section(request),
      "resolve": _resolve_section(request),
      "quota": _quota_section(request),
      "cost": _cost_section(request),
  }
  sys.stdout.write(json.dumps(result, ensure_ascii=False, separators=(",", ":")) + "\n")


def _argv_section(request):
  from src.infra import backend_models, config
  from src.runtime.hooks import backend_types

  cfg = config.CharlieBotConfig(charliebot_home="/synthetic/home")
  env_base = request["env_base"]
  results = []
  for case in request["argv_cases"]:
    option = backend_models.parse_option(
        {
            "id": case["id"],
            "label": "Codex",
            "type": "codex",
            "model": case["model"],
            **case["type_fields"],
        })
    launch = {"extra_flags": case["extra_flags"]}
    if case["resume_session_id"] is not None:
      launch["resume_session_id"] = case["resume_session_id"]
    if case["instructions_file"] is not None:
      launch["instructions_file"] = pathlib.Path(case["instructions_file"])
    if case["instructions_content"] is not None:
      launch["instructions_content"] = case["instructions_content"]
    backend = backend_types.build_backend(option, cfg, **launch)
    prepared = backend._prepare_env(dict(env_base))
    results.append(
        {
            "id": case["id"],
            "argv": backend._build_command(case["prompt"]),
            "env_delta": {
                name: value for name, value in prepared.items() if env_base.get(name) != value
            },
            "stdin": backend._stdin_prompt(case["prompt"]),
            "instructions_target": list(backend._INSTRUCTIONS_TARGET),
        })
  return results


def _env_section(request):
  from src.backends.codex import codex

  backend = codex.CodexBackend(model="gpt-5.5")
  return [{"id": case["id"], "env": backend._prepare_env(dict(case["env"]))} for case in request["env_cases"]]


def _translate_section(request):
  from src.backends.codex import codex

  backend = codex.CodexBackend(model=request["translate_model"])
  return [backend.translate_event(event) for event in request["translate_events"]]


def _read_section(request):
  from src.backends.codex import usage_logs

  signature, records = usage_logs.read(pathlib.Path(request["read_path"]), "work (default)", None)
  return {"signature": signature, "records": [_record(record) for record in records]}


def _record(record):
  fields = dataclasses.asdict(record)
  fields["kind"] = record.kind.value
  fields["sessions"] = list(record.sessions)
  return fields


class _Config:
  """The one method of the config the resolver reads: a backend option's compaction limit."""

  def __init__(self, limits):
    self._limits = limits

  def get_backend_option(self, backend_id):
    if backend_id not in self._limits:
      return None
    return SimpleNamespace(model_auto_compact_token_limit=self._limits[backend_id])


def _resolve_section(request):
  from src.backends.codex import codex_usage

  chat_root = pathlib.Path(request["chat_root"])
  results = []
  for case in request["resolve_cases"]:
    resolver = codex_usage.CodexUsageResolver(
        _Config(request["compact_limits"]),
        {},
        lambda session_id: chat_root / session_id / "chat_events.jsonl",
    )
    meta = SimpleNamespace(backend=case["backend"], cc_session_id=case["cc_session_id"])
    results.append({
        "id": case["id"],
        "usage": resolver.resolve(case["session_id"], meta, case["events"]),
    })
  return results


def _quota_section(request):
  from src.backends.codex import usage_quota
  from src.runtime.hooks import usage_sources

  now = datetime.datetime.fromtimestamp(request["quota_now_unix"], datetime.UTC)
  results = []
  for case in request["quota_cases"]:
    account = usage_quota.CodexQuotaAccount("main", case["codex_home"])
    payload = asyncio.run(account.fetch())
    marked = account.mark_expired(payload, now) if payload is not None else None
    results.append(
        {
            "id": case["id"],
            "fetch": _without_fetched_at(payload, usage_sources),
            "marked": _without_fetched_at(marked, usage_sources),
            "last_error": account.last_error,
        })
  return results


def _without_fetched_at(entry, usage_sources):
  if entry is None:
    return None
  return {key: value for key, value in entry.items() if key != usage_sources.PANEL_FETCHED_AT}


def _cost_section(request):
  from src.backends.codex import codex_pricing

  return [
      {
          "model": case["model"],
          "usage": case["usage"],
          "cost": codex_pricing.calculate_codex_usage_cost_usd(case["model"], case["usage"]),
      } for case in request["cost_cases"]
  ]


if __name__ == "__main__":
  _main()
