"""Write one tree-relative workspace file for the file-route parity scenario."""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict:
  path = b.cfg.charliebot_home.parent / "workspace" / "fixture-extra.txt"
  path.write_text("Synthetic workspace file for parity.\n", encoding="utf-8")
  return {"file": "workspace/fixture-extra.txt"}
