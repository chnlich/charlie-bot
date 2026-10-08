"""The Claude Code backend's setup check: ``setup_step`` asserts the command disallows the headless-unsafe tools."""

from src.backends.claude_code.claude_code import BASE_COMMAND
from src.infra import config


def setup_step(cfg: config.CharlieBotConfig, *, dry_run: bool) -> None:
  """Smoke-check the Claude Code backend command for headless-unsafe tools. Registered with ``register_setup_step``."""
  print("==> Checking Claude Code backend tools")
  required = ["Monitor", "ScheduleWakeup", "CronCreate", "CronDelete", "CronList"]

  try:
    disallowed_index = BASE_COMMAND.index("--disallowed-tools")
  except ValueError as exc:
    raise SystemExit("missing --disallowed-tools in BASE_COMMAND") from exc

  try:
    disallowed_tools = set(BASE_COMMAND[disallowed_index + 1].split(","))
  except IndexError as exc:
    raise SystemExit("--disallowed-tools has no value in BASE_COMMAND") from exc

  missing = set(required) - disallowed_tools
  if missing:
    raise SystemExit(f"missing disallowed tools: {','.join(sorted(missing))}")

  print("OK: backend disallows " + ",".join(required))
