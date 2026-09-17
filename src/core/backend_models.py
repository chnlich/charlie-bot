"""The backend-option and Claude-account models the config schema builds its fields from.

``src/core/config`` imports this module directly so a config read (every CLI
invocation's first ``get_config``) never constructs the session/API models in
``src.core.models``; that module re-exports these names for its established
import path.
"""

from enum import StrEnum
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter, model_validator

# ---------------------------------------------------------------------------
# Backend Models
# ---------------------------------------------------------------------------


class BackendType(StrEnum):
  """The BackendOption.type vocabulary; config.yaml carries the same strings."""

  CC_CLAUDE = "cc-claude"
  CC_KIMI = "cc-kimi"
  CC_OPENAI_COMPATIBLE = "cc-openai-compatible"
  CODEX = "codex"
  CHARLIE_CODE = "charlie-code"
  GEMINI = "gemini"
  OPENCODE = "opencode"
  ANTIGRAVITY = "antigravity"
  TUI_CLI = "tui-cli"


MODEL_OPTIONAL_ROUTING_BACKEND_TYPES: frozenset[BackendType] = frozenset({BackendType.ANTIGRAVITY, BackendType.TUI_CLI})


class BackendBase(BaseModel):
  """Fields every backend option carries; each type's own fields live on the subclasses below."""
  model_config = ConfigDict(extra='forbid')

  id: str
  label: str
  model: str | None = None
  # Overlay filename (no .md) under prompts/model_overlays/. Literal "none" =
  # explicitly fenceless (silent); None = undeclared; a declared-but-unreadable
  # file degrades the wake to a fenceless run. The two latter cases emit one
  # unified backend_overlay_inactive alert, told apart by its reason field —
  # the read failure never raises.
  prompt_overlay: str | None = None

  @model_validator(mode='after')
  def require_model(self) -> 'BackendBase':
    if self.model is None and self.type not in MODEL_OPTIONAL_ROUTING_BACKEND_TYPES:
      raise ValueError(f"backend '{self.id}' (type '{self.type}') requires 'model'")
    return self


class CcClaudeBackend(BackendBase):
  type: Literal[BackendType.CC_CLAUDE] = BackendType.CC_CLAUDE
  effort: str | None = None
  fast_mode: bool = False  # cc-claude only: enable Claude Code fast mode via --settings '{"fastMode":true}'
  cli_binary: str | None = None


class CcKimiBackend(BackendBase):
  type: Literal[BackendType.CC_KIMI] = BackendType.CC_KIMI
  credential: str


class CcOpenAICompatibleBackend(BackendBase):
  type: Literal[BackendType.CC_OPENAI_COMPATIBLE] = BackendType.CC_OPENAI_COMPATIBLE
  api_base: str  # OpenAI-compatible base URL
  credential: str | None = None


class CodexBackend(BackendBase):
  type: Literal[BackendType.CODEX] = BackendType.CODEX
  model_reasoning_effort: str | None = None  # per-backend reasoning effort override
  model_auto_compact_token_limit: int | None = Field(default=None, gt=0)  # per-backend auto-compact token limit


class CharlieCodeBackend(BackendBase):
  type: Literal[BackendType.CHARLIE_CODE] = BackendType.CHARLIE_CODE
  api_base: str | None = None  # OpenAI-compatible base URL
  context_window: int | None = Field(
      default=None, gt=0)  # compaction context window in tokens (None = charlie-code default)
  credential: str | None = None
  proxy_url: str | None = None  # per-entry HTTP/HTTPS proxy URL injected into the child env
  image_input: bool = True  # entries accept image attachments (sent as --image) by default; false refuses them (set it on text-only endpoints)
  stream: bool = True  # endpoint is called in streaming mode (default); false emits --no-stream
  timeout_seconds: int | None = Field(
      default=None,
      gt=0)  # call budget: silence bound when streaming, whole-call bound when not (None = charlie-code default)
  top_p: float | None = Field(default=None, gt=0.0, le=1.0)  # nucleus cutoff (None = charlie-code default)
  temperature: float | None = Field(default=None, ge=0.0)  # sampling temperature (None = charlie-code default)


class GeminiBackend(BackendBase):
  type: Literal[BackendType.GEMINI] = BackendType.GEMINI


class OpencodeBackend(BackendBase):
  type: Literal[BackendType.OPENCODE] = BackendType.OPENCODE
  proxy_url: str | None = None  # per-backend HTTP/HTTPS proxy URL


class AntigravityBackend(BackendBase):
  type: Literal[BackendType.ANTIGRAVITY] = BackendType.ANTIGRAVITY
  print_timeout: str | None = None  # antigravity only: agy --print turn budget (Go duration, e.g. "1h")


class TuiCliBackend(BackendBase):
  type: Literal[BackendType.TUI_CLI] = BackendType.TUI_CLI
  cli_binary: str | None = None


# One class per type: a config entry validates against the subclass its ``type``
# names, so illegal field/type combinations are unconstructable (the same
# discriminated-union pattern as src.core.models' WatchTarget).
BACKEND_CLASSES = (
    CcClaudeBackend, CcKimiBackend, CcOpenAICompatibleBackend, CodexBackend, CharlieCodeBackend, GeminiBackend,
    OpencodeBackend, AntigravityBackend, TuiCliBackend)

# Discriminated union on `type`: config.yaml entries dispatch on their type tag.
BackendOption = Annotated[
    CcClaudeBackend | CcKimiBackend | CcOpenAICompatibleBackend | CodexBackend | CharlieCodeBackend | GeminiBackend |
    OpencodeBackend | AntigravityBackend | TuiCliBackend,
    Field(discriminator="type"),
]

BACKEND_OPTION_ADAPTER = TypeAdapter(BackendOption)


def backend_type_allows_missing_model(backend_type: str) -> bool:
  return backend_type in MODEL_OPTIONAL_ROUTING_BACKEND_TYPES


class ClaudeAccount(BaseModel):
  """One Claude subscription login in the account pool (src/core/claude_accounts.py).

  ``label`` names the account in server logs and the usage panel (it follows the
  label the panel derived from the directory name before the pool existed);
  ``config_dir`` is the login's CLAUDE_CONFIG_DIR. Order carries no meaning.
  """
  model_config = ConfigDict(extra='forbid')

  label: str
  config_dir: str


class ClaudeCompactionConfig(BaseModel):
  """Context floors, in tokens, for the Sonnet compaction the pool runs on Fable sessions.

  ``relay_tokens`` applies before an account relay (the cache is cold in the new
  login anyway); ``expired_cache_tokens`` applies when a user message arrives after
  the one-hour prompt cache has expired. Below the floor a cold read is cheaper
  than a compaction, so nothing runs.
  """
  model_config = ConfigDict(extra='forbid')

  relay_tokens: int = Field(default=100_000, gt=0)
  expired_cache_tokens: int = Field(default=50_000, gt=0)
