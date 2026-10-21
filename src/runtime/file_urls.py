"""The URL form of a served file, its path on this host, and file-link scanning."""

import dataclasses
import enum
import pathlib
import re
import urllib.parse

# File-server URL prefix: the file-server package mounts its router under it.
# The prefix names what has to follow it — the absolute filesystem path with its leading `/` removed —
# so a path that dropped its leading segments reads as wrong where it is written. The legacy /files
# (and singular /file) spellings are hard-offline: nothing is mounted there, both answer 404.
# Every Python reader derives its form from this tuple. Some packages build served-file URLs;
# other packages parse them. The auth whitelist deliberately derives nothing: the file server sits
# behind the access key, and deriving the prefix there would re-open the gate. The frontend gate
# (web/static/js/chat/file-links.js) mirrors the single element.
FILE_SERVER_MOUNTS = ("/absolute_filepath",)
RETIRED_FILE_SERVER_PREFIXES = ("/files/", "/file/")

# A URL reply text can carry any scheme and host. The path names the file-server mount, while the
# port identifies whether a retired-prefix URL belongs to this server.
_URL_RE = re.compile(
    r"[a-zA-Z][a-zA-Z0-9+.\-]*://"
    r"(?:\[[^\]\s]+\]|[^/\s:()]+)"
    r"(?::(?P<port>\d+))?"
    r"(?P<path>/[^\s?#()]*)?"
    r"(?:\?[^\s#]*)?(?:#[^\s]*)?")
_URL_SCHEME_RE = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.\-]*://")
_MARKDOWN_INLINE_TARGET_RE = re.compile(r"\[[^\]\n]*\]\(\s*(?P<target><[^>\n]*>|[^)\s]+)")
_MARKDOWN_REFERENCE_TARGET_RE = re.compile(r"(?m)^[ \t]{0,3}\[[^\]\n]+\]:[ \t]*(?P<target><[^>\n]*>|[^\s]+)")
LINK_TRAILING_PUNCTUATION_RE = re.compile(r"[.,;:!?*。！？；，、：．]+$")


class FileLinkForm(str, enum.Enum):
  """The file-link form that the scanner recognized."""

  CANONICAL = "canonical"
  RETIRED_PREFIX = "retired_prefix"
  LOCAL_PATH = "local_path"


@dataclasses.dataclass(frozen=True)
class ScanContext:
  """The server, CharlieBot, user-home and session paths used to resolve links in one message."""

  server_port: int
  charliebot_home: pathlib.Path
  session_dir: pathlib.Path
  user_home: pathlib.Path


@dataclasses.dataclass(frozen=True)
class FileLink:
  """One recognized file link and the scanner's liveness and correction result."""

  link: str
  form: FileLinkForm
  path: pathlib.Path | None
  live: bool | None
  correct_link: str | None

  @property
  def dead(self) -> bool:
    """Whether this finding names a link the reader cannot open."""
    return self.live is False


@dataclasses.dataclass(frozen=True)
class _Candidate:
  start: int
  end: int
  link: str
  markdown_target: bool


def host_file(link: str | None) -> pathlib.Path | None:
  """The host file *link* names, or None when it names no file on this host.

  A file-server path names the absolute path after the mount, whatever scheme or host precedes the path. A bare
  absolute path names itself only when it has no query or fragment.
  """
  if not link:
    return None
  parts = urllib.parse.urlsplit(link)
  path = urllib.parse.unquote(parts.path)
  for mount in FILE_SERVER_MOUNTS:
    if path.startswith(mount + "/"):
      return pathlib.Path(path[len(mount):])
  if not parts.scheme and not parts.netloc and not parts.query and not parts.fragment and path.startswith("/"):
    return pathlib.Path(path)
  return None


def _file_server_named_path(url_path: str) -> str | None:
  """The decoded absolute filesystem path a canonical file-server path names."""
  for mount in FILE_SERVER_MOUNTS:
    marker = mount + "/"
    if url_path.startswith(marker):
      return "/" + urllib.parse.unquote(url_path[len(marker):])
  return None


def _trim_url_punctuation(link: str) -> str:
  return LINK_TRAILING_PUNCTUATION_RE.sub("", link)


def _candidates(text: str) -> list[_Candidate]:
  """Find URL and Markdown target spans in order, keeping a Markdown span's target status."""
  found: dict[tuple[int, int], _Candidate] = {}
  for match in _URL_RE.finditer(text):
    link = _trim_url_punctuation(match.group(0))
    if link:
      candidate = _Candidate(match.start(), match.start() + len(link), link, markdown_target=False)
      found[(candidate.start, candidate.end)] = candidate

  for pattern in (_MARKDOWN_INLINE_TARGET_RE, _MARKDOWN_REFERENCE_TARGET_RE):
    for match in pattern.finditer(text):
      raw_target = match.group("target")
      target_start = match.start("target")
      if raw_target.startswith("<") and raw_target.endswith(">"):
        raw_target = raw_target[1:-1]
        target_start += 1
      link = _trim_url_punctuation(raw_target) if _URL_SCHEME_RE.match(raw_target) else raw_target
      if not link:
        continue
      candidate = _Candidate(target_start, target_start + len(link), link, markdown_target=True)
      found[(candidate.start, candidate.end)] = candidate

  return sorted(found.values(), key=lambda candidate: (candidate.start, candidate.end))


def _retired_prefix(path: str) -> str | None:
  return next((prefix[:-1] for prefix in RETIRED_FILE_SERVER_PREFIXES if path.startswith(prefix)), None)


def _effective_port(parts: urllib.parse.SplitResult, explicit_port: int | None) -> int | None:
  if explicit_port is not None:
    return explicit_port
  return {"http": 80, "https": 443}.get(parts.scheme.lower())


def _canonical_is_local(parts: urllib.parse.SplitResult, port: int | None, context: ScanContext | None) -> bool:
  if not parts.netloc:
    return True
  host = (parts.hostname or "").lower()
  if host in ("localhost", "127.0.0.1"):
    return True
  return context is not None and port == context.server_port


def _retired_path(path: str, prefix: str, context: ScanContext | None) -> tuple[pathlib.Path | None, str | None]:
  if context is None:
    return None, None
  tail = urllib.parse.unquote(path[len(prefix) + 1:])
  absolute_path = (pathlib.Path("/") / tail).resolve()
  home = context.charliebot_home.resolve()
  home_path = (home / tail).resolve()
  candidates = [absolute_path]
  if home_path.is_relative_to(home):
    candidates.append(home_path)
  for candidate in candidates:
    if candidate.exists():
      return candidate, _encoded_file_server_url_for_parts(candidate)
  return absolute_path, None


def _encoded_file_server_url_for_parts(path: pathlib.Path) -> str:
  encoded_path = urllib.parse.quote(path.as_posix().lstrip("/"), safe="/:@")
  return f"{FILE_SERVER_MOUNTS[0]}/{encoded_path}"


def _local_correct_link(path: pathlib.Path | None) -> str | None:
  if path is None:
    return None
  return _encoded_file_server_url_for_parts(path)


def _local_path_target(parts: urllib.parse.SplitResult, context: ScanContext | None) -> pathlib.Path | None:
  path = urllib.parse.unquote(parts.path)
  if parts.scheme or parts.netloc or not path:
    return None
  if path.startswith(("?", "#")) or path.startswith("/?") or path in ("/diff", "/api") or path.startswith(
      ("/diff/", "/api/")):
    return None
  if path.startswith("~/") or not path.startswith("/"):
    if context is None:
      return None
    if path.startswith("~/"):
      return (context.user_home / path[2:]).resolve()
    return (context.session_dir / path).resolve()
  first_segment = path[1:].split("/", maxsplit=1)[0]
  if first_segment and pathlib.Path("/").joinpath(first_segment).is_dir():
    return pathlib.Path(path)
  return None


def _candidate_port(link: str) -> int | None:
  match = _URL_RE.match(link)
  if match is None or match.group("port") is None:
    return None
  return int(match.group("port"))


def _scan_candidate(candidate: _Candidate, context: ScanContext | None) -> FileLink | None:
  parts = urllib.parse.urlsplit(candidate.link)
  port = _effective_port(parts, _candidate_port(candidate.link))
  canonical_path = _file_server_named_path(parts.path)
  if canonical_path is not None:
    file_path = pathlib.Path(canonical_path)
    if not _canonical_is_local(parts, port, context):
      return FileLink(candidate.link, FileLinkForm.CANONICAL, file_path, None, None)
    live = file_path.is_file() or file_path.is_dir()
    return FileLink(candidate.link, FileLinkForm.CANONICAL, file_path, live, None)

  retired = _retired_prefix(parts.path)
  if retired is not None and (candidate.markdown_target or (context is not None and port == context.server_port)):
    file_path, correct_path = _retired_path(parts.path, retired, context)
    correct_link = None
    if correct_path is not None:
      correct_parts = urllib.parse.urlsplit(correct_path)
      correct_link = urllib.parse.urlunsplit(
          (parts.scheme, parts.netloc, correct_parts.path, parts.query, parts.fragment))
    return FileLink(candidate.link, FileLinkForm.RETIRED_PREFIX, file_path, live=False, correct_link=correct_link)

  local_path = _local_path_target(parts, context) if candidate.markdown_target else None
  if local_path is not None:
    return FileLink(
        candidate.link, FileLinkForm.LOCAL_PATH, local_path, live=False, correct_link=_local_correct_link(local_path))
  return None


def scan_file_links(text: str, context: ScanContext | None = None) -> list[FileLink]:
  """Find file links in *text* and classify their form and liveness.

  URLs with a scheme and host are scanned anywhere in the text, including code. Markdown link targets and reference
  definitions can also name local paths. The context determines this server's port and resolves path corrections.
  """
  return [finding for candidate in _candidates(text) if (finding := _scan_candidate(candidate, context)) is not None]


def dead_links_detail(findings: list[FileLink]) -> str:
  """The HTTP 422 detail for dead links found by :func:`scan_file_links`."""
  lines = ["Dead file links; nothing was recorded."]
  for finding in findings:
    if not finding.dead:
      continue
    if finding.form == FileLinkForm.RETIRED_PREFIX:
      reason = "/files/ is a retired prefix."
    elif finding.form == FileLinkForm.LOCAL_PATH:
      reason = "a local path opens nothing on the web page."
    else:
      reason = f"no file exists at {finding.path}."
    correction = "" if finding.correct_link is None else f" Correct link: {finding.correct_link}"
    lines.append(f"- {finding.link}: {reason}{correction}")
  return "\n".join(lines)
