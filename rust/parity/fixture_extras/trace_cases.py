"""Create small synthetic trace files for the trace feature parity cases."""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict[str, str]:
  directory = b.cfg.charliebot_home.parent / "workspace" / "trace-cases"
  directory.mkdir(parents=True)
  files = {
      "rank0":
          (
              "rank0.json",
              (
                  '{"traceEvents":[{"ph":"M","name":"process_labels","pid":7,"args":{"labels":"GPU 0"}},'
                  '{"ph":"X","pid":7,"tid":1,"ts":1,"dur":2,"name":"rank zero",'
                  '"args":{"stream":0,"detail":"drop in slim mode"}},'
                  '{"ph":"s","pid":7,"tid":"1","ts":2,"id":"flow","name":"flow"}]}\n'),
          ),
      "rank1":
          (
              "rank1.json",
              (
                  '{"traceEvents":[{"ph":"M","name":"process_labels","pid":8,"args":{"labels":"CPU"}},'
                  '{"ph":"X","pid":8,"tid":1,"ts":3,"dur":4,"name":"rank one"}]}\n'),
          ),
      "slim":
          (
              "slim.json",
              (
                  '{"traceEvents":[{"ph":"X","pid":1,"tid":2,"cat":"cpu_instant_event","name":"drop"},'
                  '{"ph":"X","pid":1,"tid":2,"name":"keep","args":{"stream":"copy","detail":"drop"}},'
                  '{"ph":"X","pid":1,"tid":2,"name":"remove args","args":{"detail":"drop"}}]}\n'),
          ),
      "non_trace": ("not-a-trace.json", '{"analysis":[]}\n'),
      "not_json": ("not-json.txt", "synthetic text\n"),
      "malformed": ("malformed.txt", '{"traceEvents":[\n'),
  }
  result = {"directory": "workspace/trace-cases"}
  for key, (name, content) in files.items():
    (directory / name).write_text(content, encoding="utf-8")
    result[key] = f"workspace/trace-cases/{name}"
  return result
