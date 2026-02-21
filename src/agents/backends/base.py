"""Abstract base class for agent subprocess backends."""

from abc import ABC, abstractmethod
from collections.abc import AsyncIterator
from typing import Optional


class AgentBackend(ABC):
  """Abstract interface for running a Claude agent subprocess.

  Subclasses encapsulate subprocess lifecycle management and NDJSON streaming
  so callers can consume a uniform stream of event dicts regardless of the
  underlying execution mechanism.
  """

  @property
  @abstractmethod
  def pid(self) -> Optional[int]:
    """Return the PID of the running subprocess, or None if not yet started."""
    ...

  @abstractmethod
  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Spawn the agent subprocess and yield parsed NDJSON event dicts.

    Implementations must be async generator functions.  After the generator
    is fully consumed, implementation-specific result attributes (e.g.
    ``exit_code``, ``stderr_text``) will be populated.

    Args:
      prompt: The prompt string passed as the final CLI argument.
      cwd: Working directory for the subprocess.
      env: Full environment dict for the subprocess.

    Yields:
      Parsed NDJSON event dicts from the subprocess stdout.
    """
    raise NotImplementedError
    yield  # noqa: unreachable — required to mark function as an async generator

  @abstractmethod
  async def terminate(self) -> None:
    """Terminate the subprocess gracefully (SIGTERM, then SIGKILL after timeout)."""
    ...
