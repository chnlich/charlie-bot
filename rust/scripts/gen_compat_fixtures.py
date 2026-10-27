"""Write the fixtures that the charliebot-compat and charliebot-core tests compare the Rust renderings with.

Every expected value comes from running the Python code of the baseline checkout: `src/infra/responses.py`
for orjson, Starlette's `JSONResponse` for the JSON responses, Jinja2 for the template filters, FastAPI for
the 422 bodies, `src/infra/ssh.py` for the ssh argv and CPython's `mimetypes` for the content types.

Run from the repository root; the output goes to rust/fixtures/python/:

    uv run python rust/scripts/gen_compat_fixtures.py
"""

import enum
import json
import math
import mimetypes
import os
import pathlib
import random
import struct
import sys
import tempfile
from typing import Annotated, Literal

import pydantic
from fastapi import FastAPI, Query
from fastapi.testclient import TestClient
from starlette import responses as starlette_responses
from starlette.templating import Jinja2Templates

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


def reject_blank(text: str) -> str:
  if not text.strip():
    raise ValueError("must not be blank")
  return text


class Three(enum.Enum):
  A = "a"
  B = "b"
  C = "c"


class Item(pydantic.BaseModel):
  """The body model of the body cases: one field of each validation kind the routes use."""
  model_config = pydantic.ConfigDict(extra="forbid")
  name: str = pydantic.Field(min_length=2, max_length=3)
  count: int
  ratio: float = pydantic.Field(default=0.5, gt=0.0, le=1.0)
  flag: bool = False
  tags: list[str] = []
  meta: dict[str, int] = {}
  choice: Literal["thumbs_up", "thumbs_down"] = "thumbs_up"
  note: Annotated[str, pydantic.AfterValidator(reject_blank)] = ""


def validation_app() -> FastAPI:
  """The endpoints whose parameters the Rust parsers mirror. Each parameter is declared as the routes declare it."""
  app = FastAPI()

  @app.get("/query/int")
  def query_int(v: int = Query(default=100, ge=1, le=500)) -> dict:
    return {"ok": True}

  @app.get("/query/int-ge")
  def query_int_ge(v: int = Query(default=0, ge=0)) -> dict:
    return {"ok": True}

  @app.get("/query/bool")
  def query_bool(v: bool = Query(default=False)) -> dict:
    return {"ok": True}

  @app.get("/query/literal")
  def query_literal(v: Literal["three-dot", "two-dot"] = Query("three-dot")) -> dict:
    return {"ok": True}

  @app.get("/query/enum")
  def query_enum(v: Three = Query(Three.A)) -> dict:
    return {"ok": True}

  @app.get("/query/required")
  def query_required(repo: str = Query(...)) -> dict:
    return {"ok": True}

  @app.get("/query/combined")
  def query_combined(
      repo: str = Query(...),
      limit: int = Query(default=100, ge=1, le=500),
      include_archived: bool = Query(default=False),
      mode: Literal["three-dot", "two-dot"] = Query("three-dot"),
  ) -> dict:
    return {"ok": True}

  @app.get("/path/{item_id}")
  def path_int(item_id: int) -> dict:
    return {"ok": True}

  @app.post("/body/item")
  def body_item(payload: Item) -> dict:
    return {"ok": True}

  return app


# Each endpoint's parameters as the Rust test replays them: `source` and `name` name the loc, `type` the parser,
# `required` says whether an absent value is an error, and the bounds, choices and default are FastAPI's.
VALIDATION_ENDPOINTS = {
    "/query/int": [{
        "source": "query",
        "name": "v",
        "type": "int",
        "default": 100,
        "ge": 1,
        "le": 500
    }],
    "/query/int-ge": [{
        "source": "query",
        "name": "v",
        "type": "int",
        "default": 0,
        "ge": 0
    }],
    "/query/bool": [{
        "source": "query",
        "name": "v",
        "type": "bool",
        "default": False
    }],
    "/query/literal":
        [
            {
                "source": "query",
                "name": "v",
                "type": "literal",
                "default": "three-dot",
                "choices": ["three-dot", "two-dot"]
            }
        ],
    "/query/enum": [{
        "source": "query",
        "name": "v",
        "type": "enum",
        "default": "a",
        "choices": ["a", "b", "c"]
    }],
    "/query/required": [{
        "source": "query",
        "name": "repo",
        "type": "str",
        "required": True
    }],
    "/query/combined":
        [
            {
                "source": "query",
                "name": "repo",
                "type": "str",
                "required": True
            },
            {
                "source": "query",
                "name": "limit",
                "type": "int",
                "default": 100,
                "ge": 1,
                "le": 500
            },
            {
                "source": "query",
                "name": "include_archived",
                "type": "bool",
                "default": False
            },
            {
                "source": "query",
                "name": "mode",
                "type": "literal",
                "default": "three-dot",
                "choices": ["three-dot", "two-dot"]
            },
        ],
    "/path/{item_id}": [{
        "source": "path",
        "name": "item_id",
        "type": "int",
        "required": True
    }],
}

# Query strings are percent-encoded as a client sends them; the Rust test decodes them.
QUERY_CASES = {
    "/query/int":
        [
            "v=12", "v=%2012%20", "v=%2B5", "v=1_000", "v=1.0", "v=-0", "v=0x10", "v=%D9%A1%D9%A2", "v=1e3", "v=",
            "v=%20%20", "v=-6", "v=501", "v=10000000000", "v=a%26b", "v=%097%0A", "v=1__0", "v=_1", "v=00012", "v=1.5",
            "v=1.", "v=.5", "v=1.0e2", "v=-1.0", "v=1_0.0", "v=1.000000000000000000001", "v=99999999999999999999999",
            "v=-99999999999999999999999", "v=1.0000", "v=5.0%20", "v=1_000.0", "v=1.0_0", "v=1", "v=500", "v=0",
            "v=1&v=abc", "v=abc&v=1", "", "v=2.50"
        ],
    "/query/int-ge": ["v=0", "v=-1", "v=x"],
    "/query/bool":
        [
            "v=True", "v=TRUE", "v=%20yes%20", "v=on", "v=0", "v=2", "v=", "v=oFf", "v=t", "v=N", "v=maybe", "v=1.0",
            "v=y", "v=no", "v=false", "v=1", "v=yes", "v=off", "v=f", ""
        ],
    "/query/literal": ["v=three-dot", "v=two-dot", "v=x", "v=", "v=Three-dot", ""],
    "/query/enum": ["v=b", "v=z", "v=", ""],
    "/query/required": ["repo=r", "repo=", "", "other=1"],
    "/query/combined":
        [
            "repo=r&limit=0&include_archived=maybe&mode=x",
            "limit=abc&mode=two-dot",
            "repo=r&limit=5&include_archived=true&mode=two-dot",
            "repo=r&limit=501",
            "",
        ],
    "/path/{item_id}": ["/path/abc", "/path/12", "/path/1.5", "/path/-3"],
}

# Bodies the Item model rejects, and the bodies it accepts for contrast.
BODY_CASES = {
    "missing_fields": "{}",
    "wrong_types":
        (
            '{"name": 5, "count": "x", "ratio": 0, "flag": "maybe", "tags": "t", "meta": [], '
            '"choice": "meh", "note": "n"}'),
    "name_short": '{"name": "a", "count": 1}',
    "name_long": '{"name": "abcd", "count": 1}',
    "name_number": '{"name": 1.5, "count": 1}',
    "extra_field": '{"name": "ab", "count": 1, "zzz": 1}',
    "count_fraction": '{"name": "ab", "count": 1.5}',
    "count_null": '{"name": "ab", "count": null}',
    "ratio_high": '{"name": "ab", "count": 1, "ratio": 2.0}',
    "ratio_string": '{"name": "ab", "count": 1, "ratio": "x"}',
    "ratio_list": '{"name": "ab", "count": 1, "ratio": []}',
    "ratio_tiny": '{"name": "ab", "count": 1, "ratio": 1e-08}',
    "ratio_big": '{"name": "ab", "count": 1, "ratio": 1e21}',
    "meta_bad_value": '{"name": "ab", "count": 1, "meta": {"k": "x"}}',
    "meta_fraction": '{"name": "ab", "count": 1, "meta": {"k": 1.5}}',
    "meta_null": '{"name": "ab", "count": 1, "meta": null}',
    "tags_bad_item": '{"name": "ab", "count": 1, "tags": [1]}',
    "tags_null": '{"name": "ab", "count": 1, "tags": null}',
    "choice_number": '{"name": "ab", "count": 1, "choice": 1}',
    "flag_string": '{"name": "ab", "count": 1, "flag": "maybe"}',
    "note_blank": '{"name": "ab", "count": 1, "note": "   "}',
    "list_body": "[1, 2]",
    "string_body": '"x"',
    "invalid_json_at_one": "{bad",
    "invalid_json_mid_body": '{"name": "ab",\n  "count": x}',
    "empty_body": "",
    "accepted": '{"name": "ab", "count": "12", "flag": 1, "ratio": 1.0}',
}


def validation_cases() -> dict:
  """The 422 bodies of the query, path and body cases, as the endpoints answer them."""
  client = TestClient(validation_app(), raise_server_exceptions=False)
  query_cases = []
  for template, urls in QUERY_CASES.items():
    for url in urls:
      target = url if url.startswith("/") else f"{template}?{url}"
      response = client.get(target)
      query_cases.append(
          {
              "endpoint": template,
              "url": target,
              "status": response.status_code,
              "body": response.content.decode("utf-8"),
          })
  body_cases = []
  for name, content in BODY_CASES.items():
    response = client.post("/body/item", content=content.encode("utf-8"), headers={"content-type": "application/json"})
    body_cases.append(
        {
            "name": name,
            "content": content,
            "status": response.status_code,
            "body": response.content.decode("utf-8"),
        })
  return {
      "endpoints": VALIDATION_ENDPOINTS,
      "query_cases": query_cases,
      "body_cases": body_cases,
  }


EXTRA_MIME_PATHS = [
    "/static/.hidden",
    "/static.d/noext",
    "/static/x.tar.gz",
    "/static/x.gz.gz",
    "/static/x.TGZ",
    "/static/x.SVGZ",
    "/static/x.tar.Z",
    "/static/x.html.br",
    "/static/x.UNKNOWNEXT",
    "/static/x.png.bz2",
    "/static/x.JS",
]


def mimetype_cases() -> dict:
  """The content type CPython's `mimetypes` guesses for each built-in suffix, and for paths that combine them.

  `MimeTypes()` reads only the built-in tables, the tables rust/crates/infra/compat/src/mimetypes_table.rs holds.
  """
  database = mimetypes.MimeTypes()
  suffixes = [*mimetypes._types_map_default, *mimetypes._suffix_map_default, *mimetypes._encodings_map_default]
  paths = sorted({f"/static/file{suffix}" for suffix in suffixes} | set(EXTRA_MIME_PATHS))
  return {"cases": [{"path": path, "type": database.guess_type(path)[0]} for path in paths]}


TOJSON_VALUES = [
    None,
    True,
    0,
    1.5,
    1e-05,
    1e16,
    -0.0,
    2**64 - 1,
    "a<b>&'c\"d\\e",
    "\U0001f600 é \u2028",
    [],
    {},
    {
        "b": [1, {
            "d": None
        }],
        "a": "é"
    },
    {
        "<script>": "</script>",
        "k&": ["'q'"]
    },
    [1.25, "x", False],
    {
        "z": {
            "y": {
                "x": 1
            }
        },
        "a": None
    },
]

URLENCODE_VALUES = [
    "a b/c?d=e&f#g",
    "é/中 ~_.-",
    "",
    "100%",
    42,
    1.5,
    1e-05,
    True,
    None,
    [["a", 1], ["b c", "x/y&z"]],
    {
        "k": "v w",
        "n": None
    },
    {},
    [],
]


def render_case(env, template: str, context: dict) -> dict:
  return {"template": template, "context": context, "output": env.from_string(template).render(context)}


def jinja_filter_cases() -> dict:
  """Each template rendered by the environment Starlette's Jinja2Templates builds for the page templates."""
  with tempfile.TemporaryDirectory() as directory:
    env = Jinja2Templates(directory=directory).env
    assert env.autoescape("page.html") is True and env.autoescape(None) is True, "the pages autoescape"
    cases = [render_case(env, "{{ value|tojson }}", {"value": value}) for value in TOJSON_VALUES]
    cases += [render_case(env, "{{ value|urlencode }}", {"value": value}) for value in URLENCODE_VALUES]
    cases.append(render_case(env, "{{ value|urlencode }}", {}))
    cases.append(render_case(env, "v={{ v|urlencode }}&n={{ n|tojson }}", {"v": "1.0-abc~", "n": {"b": 2, "a": 1}}))
  return {"cases": cases}


SSH_INVOCATIONS = [
    ("login.example.test", ["true"]),
    ("alice@gpu-login.example.test", ["squeue", "-u", "alice", "--format=%i %T"]),
    ("192.0.2.10", ["sh", "-c", "echo 'a b' && exit 3"]),
]


def ssh_argv_cases() -> dict:
  """The argv of `ssh_cmd` in `src/infra/ssh.py`, with its control directory under a temporary HOME.

  The module fixes its control directory when it imports, so HOME is set before the import and the directory
  `ssh_cmd` creates is removed with the temporary home. `{home}` in the fixture stands for that directory.
  """
  with tempfile.TemporaryDirectory() as home:
    os.environ["HOME"] = home
    from src.infra import ssh

    cases = [
        {
            "host": host,
            "remote_argv": remote,
            "argv": [arg.replace(home, "{home}") for arg in ssh.ssh_cmd(host, *remote)]
        } for host, remote in SSH_INVOCATIONS
    ]
  return {"cases": cases}


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
  write_fixture("validation_422", validation_cases())
  write_fixture("mimetypes", mimetype_cases())
  write_fixture("jinja_filters", jinja_filter_cases())
  write_fixture("ssh_argv", ssh_argv_cases())


if __name__ == "__main__":
  main()
