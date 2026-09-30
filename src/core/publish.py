"""The publish lane: one action both outbound surfaces call, so the URL they produce cannot diverge.

``publish_artifact`` copies an artifact into a fresh unguessable directory under
the configured publish directory (the directory the host serves publicly) and
returns the URL a reader outside the operator's devices opens. Anyone holding a
link may open it, so a link carries a random token instead of anything derived
from the page name. Preflight refuses loudly — publish lane unconfigured or not
deployed, artifact gone — and never falls back to an unpublished URL; the thread
reply path turns that refusal into a refused reply and the CLI into a non-zero
exit.
"""

import secrets
import shutil
from pathlib import Path
from typing import Self

from src.core.config import CharlieBotConfig


class PublishError(Exception):
  """A publish preflight failure; the message names the missing config key or the missing file."""


class PublishResult(str):
  """The published URL string plus the path of the published copy."""

  path: Path

  def __new__(cls, url: str, path: Path) -> Self:
    result = super().__new__(cls, url)
    result.path = path
    return result

  @property
  def url(self) -> str:
    """Return the URL as an ordinary string for callers that use the named field."""
    return str(self)


def publish_artifact(artifact: str | Path, cfg: CharlieBotConfig) -> PublishResult:
  """Copy *artifact* to ``<publish.dir>/<token>/<basename>`` and return its URL and path.

  Preflight, in order, before any write: ``publish.dir`` and
  ``publish.public_base_url`` configured (the error names the missing key); the
  publish directory present (the host's deployment step creates it);
  ``<publish.dir>/index.html`` a regular file, because the host's static server
  lists a directory without one, and a listing of the root exposes every link;
  the artifact an existing regular file. ``token`` is
  ``secrets.token_urlsafe(16)`` (22 URL-safe characters), fresh per call, so a
  second publish of the same file gets a second link and overwrites nothing.
  The token directory is chmod 0755 and the copy 0644, whatever the umask. The
  URL is ``public_base_url`` stripped of trailing slashes, then
  ``/<token>/<basename>``.
  """
  if cfg.publish.dir is None:
    raise PublishError("publish.dir is not configured; the publish lane is unavailable")
  if not cfg.publish.public_base_url:
    raise PublishError("publish.public_base_url is not configured; the publish lane is unavailable")
  if not cfg.publish.dir.is_dir():
    raise PublishError(f"publish directory does not exist: {cfg.publish.dir}")
  index = cfg.publish.dir / "index.html"
  if not index.is_file():
    raise PublishError(f"publish directory has no index.html, so its listing would expose every link: {index}")
  src = Path(artifact)
  if not src.is_file():
    raise PublishError(f"artifact is not an existing regular file: {src}")
  token = secrets.token_urlsafe(16)
  token_dir = cfg.publish.dir / token
  token_dir.mkdir()
  token_dir.chmod(0o755)
  dest = token_dir / src.name
  shutil.copyfile(src, dest)
  dest.chmod(0o644)
  return PublishResult(f"{cfg.publish.public_base_url.rstrip('/')}/{token}/{src.name}", dest)
