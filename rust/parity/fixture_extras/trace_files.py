"""Write two small Chrome trace inputs for exercising the merged-trace worker pool."""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict:
  directory = b.cfg.charliebot_home.parent / "workspace" / "traces"
  directory.mkdir(parents=True)
  files = {
      "trace-a.json": '{"traceEvents":[{"name":"parity-a","ph":"X","ts":1,"dur":2}] }\n',
      "trace-b.json": '{"traceEvents":[{"name":"parity-b","ph":"X","ts":3,"dur":4}] }\n',
  }
  for name, content in files.items():
    (directory / name).write_text(content, encoding="utf-8")
  return {"directory": "workspace/traces", "files": sorted(files)}
