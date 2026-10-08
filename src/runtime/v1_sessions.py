"""The start refusal for a home that holds v1 sessions.

A v1 session is a session whose ``metadata.json`` has no ``profile``. The session listings
validate each file and skip a file that fails, so a home with v1 sessions would start and
those sessions would vanish from the listings. ``require_no_v1_sessions`` stops the start
instead and names the conversion commands.
"""

import json
import os
from pathlib import Path

from src.infra.config import CharlieBotConfig
from src.runtime.run_identity import SESSION_METADATA_NAME

CONVERSION_SCRIPT = "scripts/v1_session_conversion.py"


def count_v1_sessions(sessions_dir: Path) -> int:
  """Count the session directories whose ``metadata.json`` has no ``profile`` or a null one.

  The scan lists *sessions_dir* once and reads only the ``profile`` key of each file, with no
  model validation. An unpublished create's staging directory (``.task-*.tmp``) is never a
  session, and a session directory without a ``metadata.json`` is not one yet. A fresh home
  has no *sessions_dir* and no sessions. A file that is not valid JSON, or is JSON but not
  an object, raises ValueError naming the path.
  """
  if not sessions_dir.is_dir():
    return 0
  count = 0
  with os.scandir(sessions_dir) as entries:
    for entry in entries:
      if not entry.is_dir() or (entry.name.startswith(".task-") and entry.name.endswith(".tmp")):
        continue
      path = Path(entry.path) / SESSION_METADATA_NAME
      try:
        raw = path.read_bytes()
      except FileNotFoundError:
        continue
      try:
        meta = json.loads(raw)
      except ValueError as e:
        raise ValueError(f"{path} is not valid JSON: {e}") from e
      if not isinstance(meta, dict):
        raise ValueError(f"{path} is not a JSON object")
      if meta.get("profile") is None:
        count += 1
  return count


def require_no_v1_sessions(cfg: CharlieBotConfig) -> None:
  """Raise ValueError when the home holds a v1 session.

  The server calls this once at start, before it serves. The error names the home, the
  count of v1 sessions and the two conversion commands.
  """
  count = count_v1_sessions(cfg.sessions_dir)
  if count > 0:
    home = cfg.charliebot_home
    noun = "session" if count == 1 else "sessions"
    raise ValueError(
        f"{home} holds {count} v1 {noun}: a v1 session's metadata.json has no profile, "
        f"and the server lists only sessions with a profile. Stop every writer of the home, "
        f"then convert it:\n"
        f"  uv run python {CONVERSION_SCRIPT} dry-run --home {home}\n"
        f"  uv run python {CONVERSION_SCRIPT} apply --home {home}")
