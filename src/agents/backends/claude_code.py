"""ClaudeCodeBackend — concrete AgentBackend wrapping the Claude Code CLI."""

from src.agents.backends.base import AgentBackend

BASE_COMMAND: list[str] = [
    "claude",
    "-p",
    "--output-format",
    "stream-json",
    "--verbose",
    "--dangerously-skip-permissions",
]


class ClaudeCodeBackend(AgentBackend):
  """Runs a Claude Code CLI subprocess and streams NDJSON events as dicts."""

  def __init__(self, **kwargs):
    model = kwargs.pop("model", None)
    system_prompt_path = kwargs.pop("system_prompt_path", None)
    extra_flags = kwargs.pop("extra_flags", None)
    super().__init__(model=model, system_prompt_path=system_prompt_path, extra_flags=extra_flags, **kwargs)
    self._cmd: list[str] = list(BASE_COMMAND)
    if model:
      self._cmd += ["--model", model]
    if system_prompt_path:
      self._cmd += ["--system-prompt", system_prompt_path]
    if extra_flags:
      self._cmd += extra_flags

  def _build_command(self, prompt: str) -> list[str]:
    return self._cmd + [prompt]
