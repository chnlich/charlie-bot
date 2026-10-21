"""Python oracle for the CLI backend types ported to ``charliebot-agents``: gemini, charlie-code, antigravity and cc-anthropic-compatible.

Reads one JSON request on stdin and writes one JSON document as the last line of stdout. The request names a synthetic
home, a synthetic CHARLIEBOT_HOME, and a PATH that holds fake gemini, charlie-code and agy binaries. HOME,
CHARLIEBOT_HOME and PATH point there before the package is imported, so the binaries and the credentials resolve inside
it. The finish section runs fake agy scripts; nothing else starts a process, and nothing makes a network call.
"""

import asyncio
import json
import os
import pathlib
import sys


def _main() -> None:
  request = json.load(sys.stdin)
  # headless_claude_env() forwards host variables from os.environ, so the host environment here is exactly the request's
  # env_base: an inherited CLAUDE_* or CHARLIEBOT_* variable of the test process would change the Python side only.
  os.environ.clear()
  os.environ.update(request["env_base"])
  os.environ["CHARLIEBOT_HOME"] = request["charliebot_home"]
  os.environ["PATH"] = request["path"]

  from src.app import registrations

  registrations.register_all()
  result = {
      "argv": _argv_section(request),
      "translate": _translate_section(request),
      "finish": _finish_section(request),
  }
  sys.stdout.write(json.dumps(result, ensure_ascii=False, separators=(",", ":")) + "\n")


def _config(request):
  from src.infra import config

  return config.CharlieBotConfig(charliebot_home=request["charliebot_home"])


def _option(case: dict):
  from src.infra import backend_models

  return backend_models.parse_option(
      {
          "id": case["id"],
          "label": "Synthetic",
          "type": case["backend_type"],
          "model": case["model"],
          **case["type_fields"],
      })


def _argv_section(request):
  from src.runtime.hooks import backend_types

  cfg = _config(request)
  results = []
  for case in request["argv_cases"]:
    launch = {"extra_flags": case["extra_flags"]}
    if case["resume_session_id"] is not None:
      launch["resume_session_id"] = case["resume_session_id"]
    if case["instructions_file"] is not None:
      launch["instructions_file"] = pathlib.Path(case["instructions_file"])
    if case["instructions_content"] is not None:
      launch["instructions_content"] = case["instructions_content"]
    backend = backend_types.build_backend(_option(case), cfg, **launch)
    backend._prepare_transport(pathlib.Path(request["transport"]))
    target = backend._INSTRUCTIONS_TARGET
    results.append(
        {
            "id": case["id"],
            "argv": backend._build_command(case["prompt"]),
            "env": backend._prepare_env(dict(request["env_base"])),
            "stdin": backend._stdin_prompt(case["prompt"]),
            "instructions_target": list(target) if target is not None else None,
        })
  return results


def _translate_section(request):
  from src.runtime.hooks import backend_types

  cfg = _config(request)
  results = {}
  for stream in request["translate_streams"]:
    backend = backend_types.build_backend(_option(stream), cfg)
    results[stream["id"]] = [backend.translate_event(event) for event in stream["events"]]
  return results


def _finish_section(request):
  from src.backends.antigravity import antigravity_cli
  from src.runtime.hooks import backend_types

  cfg = _config(request)
  results = []
  for case in request["finish_cases"]:
    case_dir = pathlib.Path(case["dir"])
    # The agy binary resolves from PATH when the backend is built, so PATH names only this case's directory.
    os.environ["PATH"] = str(case_dir)
    launch = {} if case["resume"] is None else {"resume_session_id": case["resume"]}
    backend = backend_types.build_backend(_option(case), cfg, **launch)
    events = []
    guard = None

    async def collect() -> None:
      env = {"PATH": f"{case_dir}:/usr/bin:/bin"}
      async for event in backend.run(case["prompt"], str(case_dir / "cwd"), env):
        events.append(event)

    try:
      asyncio.run(collect())
    except antigravity_cli.AgentGuardError as error:
      guard = str(error)
    results.append({"id": case["id"], "events": events, "exit_code": backend.exit_code, "guard": guard})
  return results


if __name__ == "__main__":
  _main()
