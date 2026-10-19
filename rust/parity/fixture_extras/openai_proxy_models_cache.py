"""Precreate the voice model cache that Python's asynchronous startup provisions."""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict[str, str]:
  directory = b.cfg.charliebot_home / "models"
  directory.mkdir()
  return {"directory": "home/models"}
