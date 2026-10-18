"""Write the fixtures that the charliebot-compat and charliebot-core tests compare the Rust renderings with.

Every expected value comes from running the Python code of the baseline checkout: `src/infra/responses.py`
for orjson, Starlette's `JSONResponse` for the JSON responses, Jinja2 for the template filters, FastAPI for
the 422 bodies, `src/infra/ssh.py` for the ssh argv and CPython's `mimetypes` for the content types.

Run from the repository root; the output goes to rust/fixtures/python/:

    uv run python rust/scripts/gen_compat_fixtures.py
"""

import json
import math
import pathlib
import random
import struct
import sys

from starlette import responses as starlette_responses

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURE_DIR = REPO_ROOT / "rust" / "fixtures" / "python"

sys.path.insert(0, str(REPO_ROOT))

from src.infra import responses  # noqa: E402

# Seeds the random float bit patterns, so a regeneration writes the same fixture.
FLOAT_SEED = 20261018
RANDOM_FLOAT_COUNT = 300

TRICKY_STRINGS = [
    'a"b\\c/d',
    "".join(chr(code) for code in range(32)) + "\x7f",
    "é\u2028\u2029\U0001f600",
    "中文 tab\there\nnl\rcr",
    "\x80\x9f\xa0",
    "x\x1ey",
]


def json_values() -> list[object]:
  """The values both JSON renders see: scalars, nested containers, insertion-ordered keys and floats."""
  values: list[object] = [
      None,
      True,
      False,
      0,
      -1,
      2**63 - 1,
      -(2**63),
      2**64 - 1,
      {
          "b": 1,
          "a": [None, True, False, {}, []],
          "z": {
              "y": "é"
          }
      },
      {
          "ключ": "значение",
          "emoji": "\U0001f600",
          "ctl": "\x00\x1f\x7f"
      },
      [0.0, -0.0, 5e-324, 1.7976931348623157e308, 2.2250738585072014e-308],
  ]
  values.extend(TRICKY_STRINGS)
  values.extend(
      float(f"{mantissa}e{exponent}")
      for exponent in range(-30, 31)
      for mantissa in ("1", "1.5", "9.5", "1.2345678901234567"))
  rng = random.Random(FLOAT_SEED)
  randoms: list[float] = []
  while len(randoms) < RANDOM_FLOAT_COUNT:
    candidate = struct.unpack("<d", rng.getrandbits(64).to_bytes(8, "little"))[0]
    if math.isfinite(candidate):
      randoms.append(candidate)
  values.extend(randoms)
  return values


def json_renders() -> dict:
  """Each value with the bytes of orjson's dumps and of Starlette's JSONResponse body."""
  return {
      "cases":
          [
              {
                  "value": value,
                  "orjson": responses.fast_json_bytes(value).decode("utf-8"),
                  "starlette": starlette_responses.JSONResponse(content=value).body.decode("utf-8"),
              } for value in json_values()
          ]
  }


def write_fixture(name: str, payload: dict) -> None:
  """Write one fixture file: ASCII-only JSON, so the committed bytes stay printable."""
  FIXTURE_DIR.mkdir(parents=True, exist_ok=True)
  header = {
      "generated_by": "rust/scripts/gen_compat_fixtures.py",
      "python": sys.version.split()[0],
  }
  text = json.dumps({**header, **payload}, ensure_ascii=True, indent=1) + "\n"
  (FIXTURE_DIR / f"{name}.json").write_text(text, encoding="ascii")


def main() -> None:
  write_fixture("json_renders", json_renders())


if __name__ == "__main__":
  main()
