"""The publish lane: one action both outbound surfaces call, so the URL they produce cannot diverge.

``publish_artifact`` returns the URL a reader outside the operator's devices opens, by one of two
lanes. With ``publish.command`` set, the external program receives the artifact's absolute path
and the lane options, and its one stdout line is the URL. Otherwise the directory lane copies the
artifact into a fresh unguessable directory under the configured publish directory (the directory
the host serves publicly). Anyone holding a link may open it, so a link carries a random token
instead of anything derived from the page name. Preflight refuses loudly — publish lane
unconfigured or not deployed, artifact gone, program failed — and never falls back to an
unpublished URL; the thread reply path turns that refusal into a refused reply and the CLI into a
non-zero exit.
"""

import pathlib
import secrets
import shutil
import subprocess
from collections.abc import Sequence
from typing import Self

from src.infra import config

_URL_PREFIXES = ("http://", "https://")


class PublishError(Exception):
  """A publish preflight failure; the message names the missing config key or the missing file."""


class PublishResult(str):
  """The published URL string plus the path of the local copy; None when an external program made the copy."""

  path: pathlib.Path | None

  def __new__(cls, url: str, path: pathlib.Path | None) -> Self:
    result = super().__new__(cls, url)
    result.path = path
    return result

  @property
  def url(self) -> str:
    """Return the URL as an ordinary string for callers that use the named field."""
    return str(self)


def publish_artifact(
    artifact: str | pathlib.Path, cfg: config.CharlieBotConfig, lane_options: Sequence[str] = ()) -> PublishResult:
  """Publish *artifact* by the configured lane and return its URL.

  With ``publish.command`` set, the command lane runs the program on the artifact's
  absolute path followed by *lane_options* (see ``_publish_through_command``). The
  directory lane takes no lane options.

  Directory lane: copy *artifact* to ``<publish.dir>/<token>/<basename>`` and return its URL and path.
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
  if cfg.publish.command is not None:
    return _publish_through_command(pathlib.Path(artifact), cfg.publish.command, lane_options)
  if lane_options:
    raise PublishError(
        "the directory lane takes no lane options; set publish.command to publish through an external program")
  if cfg.publish.dir is None:
    raise PublishError("publish.dir is not configured; the publish lane is unavailable")
  if not cfg.publish.public_base_url:
    raise PublishError("publish.public_base_url is not configured; the publish lane is unavailable")
  if not cfg.publish.dir.is_dir():
    raise PublishError(f"publish directory does not exist: {cfg.publish.dir}")
  index = cfg.publish.dir / "index.html"
  if not index.is_file():
    raise PublishError(f"publish directory has no index.html, so its listing would expose every link: {index}")
  src = pathlib.Path(artifact)
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


def _publish_through_command(artifact: pathlib.Path, command: list[str], lane_options: Sequence[str]) -> PublishResult:
  """Run *command* on *artifact*'s absolute path and *lane_options*; return the URL its stdout names.

  Exit 0 with exactly one stdout line that starts ``http://`` or ``https://`` is the only success.
  Every other result raises PublishError with the exit code and the program's stderr, and no URL
  is returned. The program makes its own copy, so the result carries no local path.
  """
  if not artifact.is_file():
    raise PublishError(f"artifact is not an existing regular file: {artifact}")
  argv = [*command, str(artifact.absolute()), *lane_options]
  try:
    done = subprocess.run(argv, capture_output=True, text=True, check=False)
  except OSError as e:
    raise PublishError(f"publish.command could not start {command[0]}: {e}") from e
  stderr = done.stderr.strip()
  if done.returncode != 0:
    raise PublishError(f"publish.command exited {done.returncode}; stderr: {stderr}")
  lines = done.stdout.splitlines()
  if len(lines) != 1 or not lines[0].startswith(_URL_PREFIXES):
    raise PublishError(
        f"publish.command exited {done.returncode} without exactly one http(s) URL line on stdout; "
        f"stderr: {stderr}")
  return PublishResult(lines[0], None)
