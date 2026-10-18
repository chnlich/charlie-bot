"""Write the tree the file-server scenario reads: one file per kind, a nested directory and two symbolic links."""

import base64
from urllib import parse

from rust.parity.fixture import Builder

# A one-pixel transparent PNG, so the picture carries a real PNG signature.
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")


async def build(b: Builder) -> dict:
  root = b.cfg.charliebot_home.parent / "workspace" / "files"
  nested = root / "nested"
  nested.mkdir(parents=True)
  (root / "notes.txt").write_text("Synthetic notes for the file server.\n", encoding="utf-8")
  (root / "page.html").write_text("<html><body><h1>Synthetic page</h1></body></html>\n", encoding="utf-8")
  (root / "data.json").write_text('{"synthetic": true}\n', encoding="utf-8")
  (root / "picture.png").write_bytes(PNG)
  (root / "blob.bin").write_bytes(bytes(range(256)) * 4)
  (root / "LICENSE").write_text("Synthetic license text.\n", encoding="utf-8")
  (root / "empty.txt").write_bytes(b"")
  (root / "my notes.txt").write_text("A name with a space.\n", encoding="utf-8")
  (root / "café.txt").write_text("A non-ASCII name.\n", encoding="utf-8")
  (nested / "inner.txt").write_text("Synthetic nested file.\n", encoding="utf-8")
  (root / "notes-link.txt").symlink_to("notes.txt")
  (root / "nested-link").symlink_to("nested", target_is_directory=True)
  # The scenario puts these values into request paths, so each one is percent-encoded the way a client sends it.
  paths = {
      "txt": "workspace/files/notes.txt",
      "html": "workspace/files/page.html",
      "json": "workspace/files/data.json",
      "png": "workspace/files/picture.png",
      "bin": "workspace/files/blob.bin",
      "no_extension": "workspace/files/LICENSE",
      "empty": "workspace/files/empty.txt",
      "spaced": "workspace/files/my notes.txt",
      "unicode": "workspace/files/café.txt",
      "nested_dir": "workspace/files/nested",
      "nested_file": "workspace/files/nested/inner.txt",
      "link_file": "workspace/files/notes-link.txt",
      "link_dir": "workspace/files/nested-link",
      "files_dir": "workspace/files",
  }
  return {key: parse.quote(value, safe="/") for key, value in paths.items()}
