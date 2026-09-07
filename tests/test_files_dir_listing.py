"""The file server's directory listing renders one scandir pass per request.

The reference walk restates the pre-scandir form (``Path.iterdir`` plus one
``is_dir`` and one ``stat`` per child) so the served bytes stay pinned to it:
same order (directories first, case-insensitive names), same size text, same
UTC mtime text — the walk is an optimization, not a redefinition.
"""

from __future__ import annotations

import html
import time
from datetime import UTC, datetime
from pathlib import Path
from urllib.parse import quote

from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.files import _dir_listing_html
from src.api.files import router as files_router

_TEMPLATE = """<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>Index of {display_path}</title>
<style>
  body {{ font-family: monospace; margin: 2em; }}
  table {{ border-collapse: collapse; }}
  td, th {{ padding: 4px 12px; text-align: left; }}
  a {{ text-decoration: none; color: #0366d6; }}
  a:hover {{ text-decoration: underline; }}
</style>
</head>
<body>
<h2>Index of {display_path}</h2>
<table>
<tr><th></th><th>Name</th><th>Size</th><th>Modified</th></tr>
{rows}
</table>
</body>
</html>"""


def _client() -> TestClient:
  app = FastAPI()
  app.include_router(files_router, prefix="/files")
  return TestClient(app)


def _reference_listing(dir_path: Path, url_prefix: str) -> str:
  """The pre-scandir builder, byte for byte."""
  entries: list[dict] = []
  for child in sorted(dir_path.iterdir(), key=lambda p: (not p.is_dir(), p.name.lower())):
    stat = child.stat()
    entries.append(
        {
            "name": child.name,
            "is_dir": child.is_dir(),
            "size": stat.st_size,
            "mtime": datetime.fromtimestamp(stat.st_mtime, tz=UTC),
        })
  rows = ""
  if url_prefix.rstrip("/") != "/files":
    parent = "/".join(url_prefix.rstrip("/").split("/")[:-1]) or "/files"
    rows += ('<tr>'
             f'<td>📁</td><td><a href="{html.escape(parent)}">..</a></td>'
             '<td></td><td></td>'
             '</tr>\n')
  for e in entries:
    icon = "📁" if e["is_dir"] else "📄"
    name = html.escape(e["name"] + ("/" if e["is_dir"] else ""))
    href = html.escape(f"{url_prefix.rstrip('/')}/{quote(e['name'], safe='')}")
    size = "" if e["is_dir"] else _reference_size(e["size"])
    mtime = e["mtime"].strftime("%Y-%m-%d %H:%M")
    rows += (
        f'<tr>'
        f'<td>{icon}</td><td><a href="{href}">{name}</a></td>'
        f'<td style="text-align:right">{size}</td><td>{mtime}</td>'
        f'</tr>\n')
  display_path = html.escape("/" + dir_path.as_posix().lstrip("/"))
  return _TEMPLATE.format(display_path=display_path, rows=rows)


def _reference_size(size: int) -> str:
  for unit in ("B", "KB", "MB", "GB", "TB"):
    if size < 1024:
      return f"{size:.1f} {unit}" if unit != "B" else f"{size} {unit}"
    size /= 1024
  return f"{size:.1f} PB"


def _listing_corpus(tmp_path: Path) -> Path:
  """A directory whose rows exercise the sort key, the escaping, and the size text."""
  (tmp_path / "Zeta dir").mkdir()
  (tmp_path / "alpha.txt").write_bytes(b"a" * 5)
  (tmp_path / "Beta & Co <beta>.txt").write_bytes(b"b" * 2048)
  (tmp_path / "目录.md").write_bytes("中".encode())
  (tmp_path / "note 100%.txt").write_bytes(b"x" * (1024 * 1024))
  return tmp_path


def test_listing_matches_the_reference_walk_bytes(tmp_path: Path) -> None:
  corpus = _listing_corpus(tmp_path)
  served = _dir_listing_html(corpus, f"/files{corpus}", None)
  assert served is not None
  assert served == _reference_listing(corpus, f"/files{corpus}")


def test_a_non_directory_returns_none_and_the_route_falls_through(tmp_path: Path) -> None:
  page = tmp_path / "page.txt"
  page.write_text("plain", encoding="utf-8")
  assert _dir_listing_html(page, "/files", None) is None
  response = _client().get(f"/files{page}")
  assert response.status_code == 200
  assert response.text == "plain"


def test_diff_param_on_a_directory_is_rejected(tmp_path: Path) -> None:
  response = _client().get(f"/files{tmp_path}?diff=artifacts/other.html")
  assert response.status_code == 400
  assert "not a session artifact page" in response.json()["detail"]


def test_an_absent_path_is_a_404(tmp_path: Path) -> None:
  response = _client().get(f"/files{tmp_path / 'gone'}")
  assert response.status_code == 404


def test_mtime_text_is_utc(tmp_path: Path) -> None:
  target = tmp_path / "when.txt"
  target.write_bytes(b"x")
  stamp = time.strftime("%Y-%m-%d %H:%M", time.gmtime(target.stat().st_mtime))
  served = _dir_listing_html(tmp_path, "/files", None)
  assert served is not None and stamp in served
