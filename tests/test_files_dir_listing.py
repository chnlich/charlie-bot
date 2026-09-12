"""The file server's directory listing renders one scandir pass per request.

The reference walk restates the pre-scandir form (``Path.iterdir`` plus one
``is_dir`` and one ``stat`` per child) so the served bytes stay pinned to it:
same order (directories first, case-insensitive names), same size text, same
UTC mtime text — the walk is an optimization, not a redefinition. The page
chrome itself is not restated: the reference builder formats the served
builder's own template constant.
"""

from __future__ import annotations

import html
import os
import random
import time
from datetime import UTC, datetime
from pathlib import Path
from urllib.parse import quote

from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.files import _DIR_LISTING_TEMPLATE, _dir_listing_html, _format_mtime, _listing_memo, _row_memo
from src.api.files import router as files_router


def _client() -> TestClient:
  app = FastAPI()
  app.include_router(files_router, prefix="/files")
  return TestClient(app)


def _reference_listing(dir_path: Path, url_prefix: str) -> str:
  """The pre-scandir builder, byte for byte.

  Restates only the walk and the row build; the page chrome comes from the
  served builder's own template constant.
  """
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
  return _DIR_LISTING_TEMPLATE.format(display_path=display_path, rows=rows)


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


def test_a_changed_corpus_rebuild_serves_the_cold_builds_bytes(tmp_path: Path) -> None:
  """A rebuild after a corpus move is byte-identical to a from-nothing build.

  A move between views keys the page memo miss; the rebuild renders the moved
  entry's new row and serves every unchanged row from the row memo. The page
  memo's drop models that miss without touching the corpus; the row memo stays
  warm, as a long-running server's is.
  """
  corpus = _listing_corpus(tmp_path)
  prefix = f"/files{corpus}"
  _row_memo.clear()
  _listing_memo.clear()
  assert _dir_listing_html(corpus, prefix, None) is not None
  assert len(_row_memo) == 5
  _listing_memo.clear()
  os.utime(corpus / "alpha.txt", None)
  rebuilt = _dir_listing_html(corpus, prefix, None)
  _row_memo.clear()
  _listing_memo.clear()
  cold = _dir_listing_html(corpus, prefix, None)
  assert rebuilt is not None and cold is not None
  assert rebuilt == cold


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


def test_the_diff_400_outranks_the_unreadable_403(tmp_path: Path) -> None:
  locked = tmp_path / "locked"
  locked.mkdir()
  locked.chmod(0o000)
  try:
    plain = _client().get(f"/files{locked}")
    assert plain.status_code == 403
    with_diff = _client().get(f"/files{locked}?diff=artifacts/other.html")
    assert with_diff.status_code == 400
    assert "not a session artifact page" in with_diff.json()["detail"]
  finally:
    locked.chmod(0o755)


def test_an_absent_path_is_a_404(tmp_path: Path) -> None:
  response = _client().get(f"/files{tmp_path / 'gone'}")
  assert response.status_code == 404


def test_mtime_text_is_utc(tmp_path: Path) -> None:
  target = tmp_path / "when.txt"
  target.write_bytes(b"x")
  stamp = time.strftime("%Y-%m-%d %H:%M", time.gmtime(target.stat().st_mtime))
  served = _dir_listing_html(tmp_path, "/files", None)
  assert served is not None and stamp in served


def test_mtime_formatter_matches_the_gmtime_reference() -> None:
  """_format_mtime renders strftime(gmtime(epoch)) byte-for-byte.

  The epochs pin the two roundings the integer calendar must reproduce: the
  second floor of a fractional epoch (gmtime truncates the fraction toward
  minus infinity) and the calendar carries across minute, day, month, and year
  boundaries — including the leap-day arithmetic civil-from-days rides on, the
  negative-era floor where the C reference's truncation adjustment misleads,
  and the unpadded-year renderings below year 1000.
  """
  boundaries = [
      0.0,
      0.5,
      59.9999999,
      60.0,
      3599.9999999,
      3600.0,
      86399.9999999,
      86400.0,
      -0.5,
      -1.5,
      -60.5,
      -86400.5,
      951782400.0,  # 2000-02-29 leap day
      951868799.9999999,  # its last second
      4107542400.0,  # 2100-03-01, the first non-leap century carry
      253402300799.999,  # 9999-12-31 23:59
      -30610224000.0,  # 0999-12-31, the unpadded-year rendering
      -30610223361.0,  # 1000-01-01, the four-digit carry
      -62162035200.0,  # 0000-03-01, the negative-era anchor
      -62162035201.0,  # its previous second, 0000-02-29
      -62135596800.0,  # 0000-01-01
      -62167219200.0,  # 0001-03-01 proleptic (the year -1 zone)
      -62274489600.0,  # 0001-03-01 proleptic minus one leap year
  ]
  rng = random.Random(72)
  fuzz = [rng.uniform(0, 4102444800) for _ in range(20000)]
  fuzz += [rng.uniform(-1e9, 0) for _ in range(2000)]
  fuzz += [rng.uniform(-6.3e10, 0) for _ in range(4000)]
  fuzz += [
      float(rng.randrange(0, 4102444800)) + f
      for _ in range(4000)
      for f in (rng.random(), 0.0, 0.9999999, 59.9999999, 3599.9999999)
  ]
  for epoch in boundaries + fuzz:
    assert _format_mtime(epoch) == time.strftime("%Y-%m-%d %H:%M", time.gmtime(epoch)), epoch
