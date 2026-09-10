"""The publish lane: one action both outbound surfaces call, so the URL they produce cannot diverge.

``publish_artifact`` copies an artifact into the configured publish directory (the
directory the host's 443 static lane serves) and returns the URL a reader outside the
operator's devices opens. Preflight refuses loudly — publish lane unconfigured or
artifact gone — and never falls back to an unpublished URL; the Slack reply path turns
that refusal into a refused reply and the CLI into a non-zero exit.
"""

import filecmp
import shutil
from pathlib import Path

from src.core.config import CharlieBotConfig


class PublishError(Exception):
  """A publish preflight failure; the message names the missing config key or the missing artifact."""


class PublishResult(str):
  """The published URL string plus the metadata the callers print or inspect."""

  path: Path
  overwrote: bool

  def __new__(cls, url: str, path: Path, overwrote: bool) -> "PublishResult":
    result = super().__new__(cls, url)
    result.path = path
    result.overwrote = overwrote
    return result

  @property
  def url(self) -> str:
    """Return the URL as an ordinary string for callers that use the named field."""
    return str(self)


def publish_artifact(artifact: str | Path, cfg: CharlieBotConfig) -> PublishResult:
  """Copy *artifact* into ``cfg.publish.dir`` and return the published file and URL.

  Preflight, in order: both ``publish.dir`` and ``publish.public_base_url``
  configured (the error names the missing key), the publish directory present —
  the host's deployment step creates it (a missing one means the 443 lane serves
  nothing, so the copy refuses instead of producing links that dead-end) — and
  the artifact an existing regular file (the error names the path). The copy
  lands at ``<publish.dir>/<basename>``, mode 0644, overwriting an existing file
  of that name; ``overwrote`` reports a replacement whose content differed, so
  the caller can surface the collision. The URL is ``publish.public_base_url``
  joined to the basename by a single ``/``, however many trailing slashes the
  base carries.
  """
  if cfg.publish.dir is None:
    raise PublishError("publish.dir is not configured; the publish lane is unavailable")
  if not cfg.publish.public_base_url:
    raise PublishError("publish.public_base_url is not configured; the publish lane is unavailable")
  if not cfg.publish.dir.is_dir():
    raise PublishError(f"publish directory does not exist: {cfg.publish.dir}")
  src = Path(artifact)
  if not src.is_file():
    raise PublishError(f"artifact is not an existing regular file: {src}")
  dest = cfg.publish.dir / src.name
  overwrote = dest.is_file() and not filecmp.cmp(src, dest, shallow=False)
  shutil.copyfile(src, dest)
  dest.chmod(0o644)
  return PublishResult(f"{cfg.publish.public_base_url.rstrip('/')}/{src.name}", dest, overwrote)
