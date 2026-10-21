"""The URL form of a served file and its path on this host."""

import pathlib
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
