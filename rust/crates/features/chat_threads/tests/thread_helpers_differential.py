"""Compare the thread core's pure helpers between Python and Rust, case by case.

The script writes one cases file of synthetic inputs. Python's `thread_entry` helpers answer them in `<out>/python/`,
and the Rust example `thread_helpers` answers the same file in `<out>/rust/`. Both sides use the Slack and Discord
platform constants of their own code. Every answer must match.

A control run then plants one difference in a copy of the Rust answers and compares again: the comparison must report
exactly that one difference, or the comparison itself is broken.

Usage: uv run python rust/crates/features/chat_threads/tests/thread_helpers_differential.py [--out DIR] [--binary PATH]
Exit status 0 when the Python and Rust answers agree and the control run reports exactly one difference.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import shutil
import subprocess
import sys
import types

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))

from src.features.chat_threads import thread_entry  # noqa: E402
from src.features.discord import discord_listener  # noqa: E402
from src.features.slack import slack_listener  # noqa: E402

DEFAULT_BINARY = ROOT / "rust/target/release/examples/thread_helpers"
PLATFORMS = {"slack": slack_listener.SLACK, "discord": discord_listener.DISCORD}
SLACK_TS = "1700000000.000100"
DISCORD_ID = "1234567890123456789"


def chunk_cases() -> list[dict]:
  """Text at and over the limit, newlines at the cut, CJK, emoji, and a word longer than the limit."""
  cases = []
  for name, platform in PLATFORMS.items():
    limit = platform.max_post_chars
    cjk_paragraph = "这是一个用于测试分段的中文段落，包含标点。" * 3
    texts = [
        "a" * limit,
        "a" * (limit + 1),
        ("line of text\n" * (limit // 8 + 3)),
        "para one\n\n" * (limit // 9 + 2) + "tail",
        (cjk_paragraph + "\n\n") * (limit // 60 + 2),
        "😀" * (limit + 5),
        "x" * (2 * limit + 3),
        "word " * (limit // 2) + "longword" * 7,
        "   \n\n   ",
        "",
    ]
    cases += [{"platform": name, "text": text, "limit": limit} for text in texts]
    # Small limits make the cut land inside each shape above.
    for small in (1, 7, 20):
      for text in ("first paragraph\n\nsecond paragraph\n\nthird", "中文字符很多的段落\n第二行很长很长很长", "😀😀😀😀😀\n😀"):
        cases.append({"platform": name, "text": text, "limit": small})
  return cases


def wake_label_cases() -> list[dict]:
  return [{"platform": name, "floor": floor} for name in PLATFORMS for floor in (SLACK_TS, DISCORD_ID, "42.5")]


def floor_cases() -> list[dict]:
  labels = [
      f"slack-thread-follow floor={SLACK_TS}",
      "discord-thread-follow floor=1234567890123456789",
      "no floor on this label",
      "floor=not-numeric",
      "x floor=12.3 and floor=45.6",
      "floor=99.1 extra",
  ]
  return [{"label": label} for label in labels]


def event_fixtures(platform_name: str) -> list[dict]:
  """A log with summons, a nudge, another platform's block, a bare message and a second thread's summon."""
  other = "discord" if platform_name == "slack" else "slack"
  return [
      {
          "id": "b1",
          "type": "agent_message"
      },
      {
          "id": "s1",
          "type": "agent_message",
          platform_name: {
              "thread": "t1"
          }
      },
      {
          "id": "s2",
          "type": "agent_message",
          platform_name: {
              "thread": "t1",
              "nudge_of": "s1"
          }
      },
      {
          "id": "s3",
          "type": "agent_message",
          other: {
              "thread": "t1"
          }
      },
      {
          "id": "s4",
          "type": "agent_message",
          platform_name: {
              "thread": "t2"
          }
      },
      {
          "id": "r1",
          "type": f"{platform_name}_reply",
          f"{platform_name}_reply": {
              "answers": "s1"
          }
      },
      {
          "id": "r2",
          "type": f"{platform_name}_reply",
          f"{platform_name}_reply": {
              "answers": None
          }
      },
      {
          "id": "x1",
          "type": "assistant_error",
          f"{platform_name}_notice": {
              "input_event_id": "s1"
          }
      },
      {
          "id": "d1",
          "type": "master_done",
          "input_event_ids": ["s1"]
      },
      {
          "id": "d2",
          "type": "master_done",
          "input_event_id": "s3"
      },
      {
          "id": "m1",
          "type": "assistant_error",
          f"{platform_name}_backfill": {
              "input_event_id": "s2"
          }
      },
      {
          "id": "m2",
          "type": "assistant_error",
          f"{other}_backfill": {
              "input_event_id": "s4"
          }
      },
  ]


def event_helper_cases() -> dict[str, list[dict]]:
  cases: dict[str, list[dict]] = {
      "newest_thread_input": [],
      "replied": [],
      "nudged": [],
      "noticed": [],
      "lost_summons": []
  }
  for name in PLATFORMS:
    events = event_fixtures(name)
    for event_ids in (["b1"], ["s1", "s2", "s3"], ["s1", "s2", "s3", "s4"], ["s9"], []):
      cases["newest_thread_input"].append({"platform": name, "events": events, "event_ids": event_ids})
    # A scalar under the platform key is not a summon block: the newest dict block among the ids binds, not the scalar.
    scalar_block = events + [{"id": "s5", "type": "agent_message", name: "not-a-block"}]
    cases["newest_thread_input"].append({"platform": name, "events": scalar_block, "event_ids": ["s4", "s5"]})
    for summon_id in ("s1", "s2", "s4", "s9"):
      for helper in ("replied", "nudged", "noticed"):
        cases[helper].append({"platform": name, "events": events, "summon_id": summon_id})
    for owned, running in ((["s3"], []), ([], ["s4"]), (["s3"], ["s4"]), ([], [])):
      cases["lost_summons"].append({"platform": name, "events": events, "owned": owned, "running": running})
  return cases


def unread_cases() -> list[dict]:
  numeric = [
      {
          "id": "98",
          "human": True
      }, {
          "id": "99",
          "human": True
      }, {
          "id": "100",
          "human": True
      }, {
          "id": "101",
          "human": False
      }
  ]
  slack = [
      {
          "id": "1700000000.000090",
          "human": True
      },
      {
          "id": "1700000000.000100",
          "human": True
      },
      {
          "id": "1700000000.000110",
          "human": False
      },
      {
          "id": "1700000000.000120",
          "human": True
      },
  ]
  cases = []
  for watermark in (None, "99", "100", "100"):
    cases.append({"platform": "discord", "messages": numeric, "watermark": watermark})
  for watermark in (None, "1700000000.000090", "1700000000.000100"):
    cases.append({"platform": "slack", "messages": slack, "watermark": watermark})
  return cases


def link_cases() -> dict[str, list[dict]]:
  link = "http://localhost:18498/absolute_filepath/home/u/caf%C3%A9/page.html"
  later = "https://charliebot.example/absolute_filepath/home/u/other.html"
  portless = "https://charliebot.example/absolute_filepath/home/u/page.html"
  published = "https://publish.example/Ab3dEf6hIj8kLm1nOp2q/page.html"
  return {
      "assert_no_file_server_links":
          [
              {
                  "text": f"see {link} or {later}"
              },
              {
                  "text": f"see {portless}"
              },
              {
                  "text": f"open {published} only"
              },
              {
                  "text": "plain text, no link at all"
              },
              {
                  "text": "https://host.example/absolute_filepath"
              },
              {
                  "text": "see http://localhost:١٨٤٩٨/absolute_filepath/home/u/private.txt"
              },
          ],
      "application_route_links":
          [
              {
                  "text": f"open {published} and http://127.0.0.1:8080/diff",
                  "port": 8080
              },
              {
                  "text": "http://h:8080/a and http://h:8080/a and http://h:9090/b",
                  "port": 8080
              },
              {
                  "text": "http://h:08080/x and http://h/none",
                  "port": 8080
              },
              {
                  "text": "http://h:٨٠٨٠/unicode-port",
                  "port": 8080
              },
              {
                  "text": "nothing here",
                  "port": 8080
              },
          ],
      "operator_only_note": [{
          "links": []
      }, {
          "links": ["http://127.0.0.1:8080/diff"]
      }, {
          "links": ["a", "b"]
      }],
  }


def cases_file() -> dict:
  cases: dict = {
      "chunk_text": chunk_cases(),
      "follow_wake_label": wake_label_cases(),
      "follow_floor": floor_cases(),
      "unread_after": unread_cases(),
  }
  cases.update(event_helper_cases())
  cases.update(link_cases())
  return cases


def python_answer(helper: str, case: dict) -> object:
  platform = PLATFORMS.get(case.get("platform"))
  if helper == "chunk_text":
    return thread_entry.chunk_text(case["text"], case["limit"])
  if helper == "follow_wake_label":
    return thread_entry.follow_wake_label(platform, case["floor"])
  if helper == "follow_floor":
    return thread_entry.follow_floor(case["label"])
  if helper == "newest_thread_input":
    bound = thread_entry.newest_thread_input(platform, case["events"], case["event_ids"])
    return None if bound is None else [bound[0], bound[1]]
  if helper == "replied":
    return thread_entry.replied(platform, case["events"], case["summon_id"])
  if helper == "nudged":
    return thread_entry.nudged(platform, case["events"], case["summon_id"])
  if helper == "noticed":
    return thread_entry.noticed(platform, case["events"], case["summon_id"])
  if helper == "lost_summons":
    return thread_entry.lost_summons(platform, case["events"], owned=set(case["owned"]), running=set(case["running"]))
  if helper == "unread_after":
    unread = thread_entry.unread_after(
        case["messages"],
        eligible=lambda message: message["human"],
        message_id=lambda message: message["id"],
        watermark=case["watermark"],
        id_key=platform.id_key,
    )
    return [message["id"] for message in unread]
  if helper == "assert_no_file_server_links":
    try:
      thread_entry.assert_no_file_server_links(case["text"])
    except thread_entry.ThreadReplyError as error:
      return {"refused": True, "status": int(error.status), "detail": error.detail}
    return {"refused": False}
  if helper == "application_route_links":
    config = types.SimpleNamespace(server=types.SimpleNamespace(port=case["port"]))
    return thread_entry.application_route_links(case["text"], config)
  if helper == "operator_only_note":
    return thread_entry.operator_only_note(case["links"])
  raise ValueError(f"no Python helper for {helper}")


def python_results(cases: dict) -> dict:
  """The Python answers in the JSON shape the Rust side writes, so tuples and lists compare as the same."""
  results = {helper: [python_answer(helper, case) for case in items] for helper, items in cases.items()}
  return json.loads(json.dumps(results))


def run_rust(binary: pathlib.Path, cases_path: pathlib.Path, out_dir: pathlib.Path) -> dict:
  subprocess.run([str(binary), str(cases_path), str(out_dir)], check=True)
  return json.loads((out_dir / "results.json").read_text(encoding="utf-8"))


def differences(python: dict, rust: dict) -> list[tuple[str, int, object, object]]:
  found = []
  for helper in python:
    left, right = python[helper], rust.get(helper, [])
    if len(left) != len(right):
      found.append((helper, -1, len(left), len(right)))
    for index, (a, b) in enumerate(zip(left, right)):
      if a != b:
        found.append((helper, index, a, b))
  return found


def shown(value: object) -> str:
  """The value's repr, cut short so one long answer does not bury the summary."""
  text = repr(value)
  return text if len(text) <= 160 else text[:157] + "..."


def summary(python: dict, found: list) -> str:
  lines = [f"{'helper':<30}{'cases':>7}{'differences':>13}"]
  for helper in python:
    count = sum(1 for entry in found if entry[0] == helper)
    lines.append(f"{helper:<30}{len(python[helper]):>7}{count:>13}")
  lines.append(f"{'total':<30}{sum(len(v) for v in python.values()):>7}{len(found):>13}")
  return "\n".join(lines)


def main() -> int:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument("--out", type=pathlib.Path, default=ROOT / "rust/target/thread_helpers_differential")
  parser.add_argument("--binary", type=pathlib.Path, default=DEFAULT_BINARY)
  args = parser.parse_args()
  out = args.out.resolve()
  if out.exists():
    shutil.rmtree(out)
  (out / "python").mkdir(parents=True)
  cases = cases_file()
  cases_path = out / "cases.json"
  cases_path.write_text(json.dumps(cases, ensure_ascii=False, indent=1), encoding="utf-8")

  python = python_results(cases)
  (out / "python" / "results.json").write_text(json.dumps(python, ensure_ascii=False, indent=1), encoding="utf-8")
  rust = run_rust(args.binary, cases_path, out / "rust")

  found = differences(python, rust)
  print("run: thread_helpers_differential (python vs rust)")
  print(summary(python, found))
  for helper, index, left, right in found[:20]:
    print(f"  difference {helper}[{index}]: python={shown(left)} rust={shown(right)}")

  # The control run: one planted difference in a copy of the Rust answers, which the comparison must report.
  planted = json.loads(json.dumps(rust))
  planted["chunk_text"][0][0] = planted["chunk_text"][0][0] + "!"
  control_found = differences(python, planted)
  print("control: one planted difference (chunk_text[0] gains a character)")
  print(summary(python, control_found))
  for helper, index, left, right in control_found:
    print(f"  difference {helper}[{index}]: python={shown(left)} rust={shown(right)}")

  ok = not found and len(control_found) == 1
  print("result:", "PASS" if ok else "FAIL")
  return 0 if ok else 1


if __name__ == "__main__":
  sys.exit(main())
