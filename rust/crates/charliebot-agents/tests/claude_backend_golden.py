"""Python oracle for the cc-claude factory's argv and environment contract."""

from __future__ import annotations

import json
import os
import sys

from src.app import registrations
from src.backends.claude_code.claude_config import ClaudeAccount
from src.infra import backend_models, config
from src.runtime.hooks import backend_types


def main() -> None:
  registrations.register_all()
  cases = json.load(sys.stdin)
  results = []
  for case in cases:
    inherited = case["env"]
    os.environ["HOME"] = inherited["HOME"]
    for name in (
        "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
        "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT",
    ):
      os.environ.pop(name, None)
      if name in inherited:
        os.environ[name] = inherited[name]
    option_values = {
        "id": case["id"],
        "label": case["label"],
        "type": "cc-claude",
        "model": case["model"],
        **case["type_fields"],
    }
    option = backend_models.parse_option(option_values)
    kwargs = {"extra_flags": case["extra_flags"], "instructions_file": case["instructions_file"]}
    if case["claude_session_id"] is not None:
      kwargs["claude_session_id"] = case["claude_session_id"]
    if case["claude_account"] is not None:
      kwargs["claude_account"] = ClaudeAccount(**case["claude_account"])
    backend = backend_types.build_backend(option, config.CharlieBotConfig(charliebot_home="/synthetic/home"), **kwargs)
    prepared = backend._prepare_env(dict(inherited))
    env_delta = {name: value for name, value in prepared.items() if inherited.get(name) != value}
    results.append({
        "argv": backend._build_command(case["prompt"]),
        "env_delta": env_delta,
        "stdin": backend._stdin_prompt(case["prompt"]),
        "instructions_target": list(backend._INSTRUCTIONS_TARGET),
    })
  json.dump(results, sys.stdout, ensure_ascii=False, separators=(",", ":"))
  sys.stdout.write("\n")


if __name__ == "__main__":
  main()
