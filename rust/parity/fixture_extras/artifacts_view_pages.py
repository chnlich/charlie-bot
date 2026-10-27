"""Write session artifact versions and plain files for the artifact-view parity scenario."""

from pathlib import Path

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict:
  session = await b.node("artifacts_view", "Artifact view parity")
  artifact_dir = b.cfg.sessions_dir / session / "artifacts"
  artifact_dir.mkdir(parents=True)
  (artifact_dir / "plan_01_v1.html").write_text(
      "<html><body><main><p>Keep the original words.</p></main></body></html>", encoding="utf-8")
  (artifact_dir / "plan_01_v2.html").write_text(
      "<html><body><main><p>Keep the revised words.</p></main></body></html>", encoding="utf-8")
  (artifact_dir / "trace.bin").write_bytes(bytes(range(256)) * ((2 << 20) // 256) + b"tail")
  (artifact_dir / "directory").mkdir()
  (b.cfg.sessions_dir / session / "notes").mkdir()
  (b.cfg.sessions_dir / session / "notes" / "plan_01.html").write_text(
      "<html><body><p>A page outside artifacts.</p></body></html>", encoding="utf-8")
  outside = b.cfg.charliebot_home.parent / "workspace" / "artifacts" / "outside.html"
  outside.parent.mkdir(parents=True)
  outside.write_text("<html><body><p>Outside any session.</p></body></html>", encoding="utf-8")
  return {}
