"""Python oracle for complete agent child environments across launch paths."""

from __future__ import annotations

import importlib
import json
import os
import pathlib
import sys


def reset_server_environment(case: dict[str, object]) -> None:
  process_env = case["process_env"]
  assert isinstance(process_env, dict)
  os.environ.clear()
  os.environ.update({str(name): str(value) for name, value in process_env.items()})
  package = sys.modules.get("src")
  if package is None:
    import src

    return
  importlib.reload(package)


def sorted_env(env: dict[str, str]) -> dict[str, str]:
  return dict(sorted(env.items()))


def main() -> None:
  cases: list[dict[str, object]] = json.load(sys.stdin)
  reset_server_environment(cases[0])

  from src.app import registrations

  registrations.register_all()

  from src.infra import backend_models, config
  from src.runtime import agent_environment, launch_loop
  from src.runtime.hooks import backend_types

  results = []
  for case in cases:
    reset_server_environment(case)
    process_env = dict(os.environ)
    venv_bin = pathlib.Path(str(case["venv_bin"]))
    base_env = agent_environment.agent_environment(process_env, venv_bin)
    server_bin_dir = str(case["server_bin_dir"])
    replaced_entries = (server_bin_dir, str(agent_environment.ENTRY_POINT_DIR))
    path_entries = [entry for entry in base_env["PATH"].split(os.pathsep) if entry not in replaced_entries]
    base_env["PATH"] = os.pathsep.join([server_bin_dir, *path_entries])
    os.environ.clear()
    os.environ.update(base_env)

    backend_type = str(case["backend_type"])
    model = "claude-sonnet-4-5" if backend_type == "cc-claude" else "gpt-5.5"
    option = backend_models.parse_option(
        {
            "id": f"synthetic-{backend_type}",
            "label": "Synthetic backend",
            "type": backend_type,
            "model": model,
        })
    backend = backend_types.build_backend(option, config.CharlieBotConfig(charliebot_home="/synthetic/home"))
    launch_kind = str(case["launch_kind"])

    if launch_kind == "tool-capable-one-shot":
      child_env = backend._prepare_env(dict(base_env))
    elif launch_kind == "cli-native-one-shot" and backend_type == "cc-claude":
      child_env = dict(base_env)
    elif launch_kind == "cli-native-one-shot" and backend_type == "codex":
      child_env = backend._prepare_env(dict(base_env))
    elif launch_kind == "manager-turn":
      child_env = launch_loop.child_env(os.environ)
      child_env["CHARLIEBOT_SESSION_ID"] = "current-session"
      child_env["GIT_CEILING_DIRECTORIES"] = "/synthetic/home"
      child_env["RUN_ADDITION"] = "manager-turn"
      child_env = backend._prepare_env(child_env)
    elif launch_kind == "compaction" and backend_type == "cc-claude":
      from src.backends.claude_code import claude_compaction

      # This synthetic session compacts during a manager turn, as master_cc_relay passes ctx.run_kind.
      child_env = claude_compaction.compaction_env("/synthetic/home/accounts/claude-one", "manager_turn")
    else:
      raise ValueError(f"unsupported launch kind/backend pair: {launch_kind}/{backend_type}")

    results.append({"id": case["id"], "base_env": sorted_env(base_env), "child_env": sorted_env(child_env)})

  json.dump(results, sys.stdout, ensure_ascii=False, separators=(",", ":"))
  sys.stdout.write("\n")


if __name__ == "__main__":
  main()
