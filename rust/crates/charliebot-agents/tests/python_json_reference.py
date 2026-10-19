"""The CPython side of the python_json differential test.

Prints one JSON line per corpus input with what ``json.loads`` does to it: the decoded value as ``json.dumps``
renders it, the ``str(ValueError)`` text, or ``unrepresentable`` for a document whose value a serde_json tree cannot
hold (a non-finite float, an integer beyond 64 bits, a lone surrogate). The corpus is deterministic: hand-written
documents that reach each JSONDecodeError message, invalid and wide-codec bodies, and every truncation, deletion,
insertion and single-byte substitution of a few valid documents.
"""

import json
import math
import sys

HAND_WRITTEN_BYTES = [
    b"",
    b"   ",
    b"x",
    b"nul",
    b"null x",
    b"[1,]",
    b"[1",
    b"[1 2]",
    b"[",
    b"[,]",
    b"[1,,]",
    b"[1 ,]",
    b"[1,\n2,\n]",
    b"[[1,]]",
    b"{",
    b'{"a"',
    b'{"a":',
    b'{"a":1',
    b'{"a" 1}',
    b'{"a" , 1}',
    b'{1:2}',
    b'{"a":1,}',
    b'{"a":1 ,}',
    b'{"a":1,"b":2,}',
    b'{"a":1,\n  }',
    b'{"a":1,1}',
    b'{"a":1 "b":2}',
    b'{,}',
    b'{"a":{"b":1,}}',
    b'{"a":1,"b"}',
    b'{"a":1,"b":}',
    b'{"a":[1,}',
    b'{"k": tru}',
    b'{"k" "v"}',
    b'["a\x01"]',
    b'"abc',
    b'"\\x"',
    b'"\\u12"',
    b'"\\u12G4"',
    b'"\\u0041',
    b'"\\u0041"',
    b'"\\u00e9" x',
    b'"\\ud800\\u12G4"',
    b'"\\ud83d\\u0041x',
    b'"\\ud83d\\u00G1"',
    b'"\\ud83d\\ude00"',
    b'"\\ud83d\\',
    b'"\\ud83d\\u',
    b'"\\"',
    b'"\\',
    b'"\\u',
    b'"ab\\"',
    b'"a\nb"',
    b'"\x7f"',
    b'"\\u0000"',
    b'"\\/\\b"',
    b'[1]x',
    b'[1]\x00',
    b'{"a":1}}',
    b'{"a":1}\n\n',
    b'01',
    b'-',
    b'-x',
    b'1.',
    b'2.',
    b'1e',
    b'1e+',
    b'.5',
    b'-1.5E+3x',
    b'[1e5, 1E-2, -0, 0.5]',
    b'{"a" : [ ] , "b" : { } }',
    b'\t\n\r {}\x0b',
    b'tru',
    b'[tru]',
    b"NaN",
    b"-Infinity",
    b"[NaN, }",
    b"1e400",
    b"123456789012345678901234567890",
    b"-9223372036854775809",
    b"18446744073709551616",
    b"18446744073709551615",
]

BOM_AND_WIDE_BYTES = [
    b"\xef\xbb\xbf{}",
    b"\xef\xbb\xbf\xef\xbb\xbf{}",
    b"\xef\xbb\xbf",
    b"\xef\xbb\xbf\"x",
    b"\xef\xbb",
    b"\xfe\xff\x00{\x00}",
    b"\xff\xfe{\x00}\x00",
    b"\x00\x00\xfe\xff",
    b"\xff\xfe\x00\x00{\x00\x00\x00}\x00\x00\x00",
    b"\x00{",
    b"\x00 ",
    b"\x00\x00",
    b"a\x00",
    b"[\x00]",
    b"{\x00}",
    b"{\x00\x00\x00}",
    b"\x00\x00\x00{",
    b"\x00\x00\x00\x00",
    b"\x00\x00\x00\x00\x00\x00\x00",
    b"\x00\x00\x11\x00\x00\x00",
    b"\x00\x00\x00{\x00\x00\x00}",
    b"\x00\x00\xfe\xff\x00\x11\x00\x00",
    b"\xff\xfe\x00\x00\x00\x00\x11\x00",
    b"\x00{\x00}\x00",
    b"\xd8\x00\x00\x00",
    b"\xff\xfe{\x00\x00",
    b"\xfe\xff\x00{\x00",
]

UTF8_BYTES = [
    b"\xff",
    b"\xc0\x80",
    b"\xc3",
    b'"\xc3"',
    b'"\xe2\x82"',
    b'"\xe2\x82A"',
    b"\xe2\x82",
    b"\xf0\x9f\x98",
    b'"a\xff"',
    b"\xf4\x90\x80\x80",
    b'"\xe0\x80"',
    b"\xed\xa0\x80",
    b'"\xed\xa0\x80"',
    b'"\xf0\x9f\x98\x80"',
    b'{"a":"\xc3\xa9"}',
    b"\x80",
    b"[1,\xff]",
]

TEXT_INPUTS = [
    "﻿{}",
    "﻿",
    "x﻿",
    "{}",
    '["é"]',
]

BASES = [
    b'{"a":[1,2.5,-0,"x\\u00e9\\ud83d\\ude00",true,false,null],"b":{"c":"d\\n"},"e":[]}',
    b'[{"k":"v"},[1e5,1E-2],"\\/\\b\\f\\r\\t"]',
    b'  {"messages":[{"role":"user","content":"hi"}],"max_tokens":16}  ',
    b'"\\ud83d\\ude00"',
    b"-0.5e+3",
]

INSERTED = [
    b'"',
    b"\\",
    b",",
    b":",
    b"}",
    b"]",
    b"{",
    b"[",
    b"u",
    b"x",
    b"0",
    b"1",
    b"e",
    b".",
    b" ",
    b"\n",
    b"\x01",
    b"\xff",
    b"\x7f",
    b"N",
    b"t",
    b"-",
    b"\xc3",
]


def mutations(base):
  """Every prefix, deletion, insertion and single-byte substitution of *base*, in a fixed order."""
  for cut in range(len(base) + 1):
    yield base[:cut]
  for at in range(len(base)):
    yield base[:at] + base[at + 1:]
  for at in range(len(base) + 1):
    for inserted in INSERTED:
      yield base[:at] + inserted + base[at:]
  for at in range(len(base)):
    for inserted in INSERTED:
      yield base[:at] + inserted + base[at + 1:]


def is_unrepresentable(value):
  stack = [value]
  while stack:
    item = stack.pop()
    if isinstance(item, float):
      if not math.isfinite(item):
        return True
    elif isinstance(item, int) and not isinstance(item, bool):
      if not -(2**63) <= item < 2**64:
        return True
    elif isinstance(item, str):
      if any(0xD800 <= ord(character) <= 0xDFFF for character in item):
        return True
    elif isinstance(item, list):
      stack.extend(item)
    elif isinstance(item, dict):
      for key, member in item.items():
        stack.append(key)
        stack.append(member)
  return False


def outcome(document):
  try:
    value = json.loads(document)
  except ValueError as error:
    return {"outcome": "error", "text": str(error)}
  if is_unrepresentable(value):
    return {"outcome": "unrepresentable", "text": ""}
  return {"outcome": "value", "text": json.dumps(value, ensure_ascii=False, separators=(",", ":"))}


def corpus():
  for document in HAND_WRITTEN_BYTES + BOM_AND_WIDE_BYTES + UTF8_BYTES:
    yield "bytes", document
  for base in BASES:
    for document in mutations(base):
      yield "bytes", document
  for text in TEXT_INPUTS:
    yield "text", text


def main():
  for mode, document in corpus():
    record = outcome(document)
    if mode == "bytes":
      record["input"] = document.hex()
    else:
      record["input"] = document
    record["mode"] = mode
    sys.stdout.write(json.dumps(record) + "\n")


if __name__ == "__main__":
  main()
