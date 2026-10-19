"""Compares the Python and Rust backups of one home through Python's tarfile.

Usage: differential_compare.py <home> <rust_archive> [--plant-difference]

The driver creates Python's archive of the home beside it, then compares the member list, each member's
metadata and PAX records, and each regular member's content. --plant-difference changes one Rust member's
content before the comparison, the control run that must report exactly one difference.
"""

import argparse
import os
import pathlib
import sys
import tarfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[5] / "src"))

parser = argparse.ArgumentParser(description="Compares the Python and Rust backups of one home.")
parser.add_argument("home", type=pathlib.Path)
parser.add_argument("rust_archive", type=pathlib.Path)
parser.add_argument("--plant-difference", action="store_true")
args = parser.parse_args()

os.environ["CHARLIEBOT_HOME"] = str(args.home)
from src.features.backup import backup  # noqa: E402

python_archive = backup.create_backup()


def load(archive_path: pathlib.Path) -> tuple[list[tarfile.TarInfo], dict[str, bytes | None]]:
  with tarfile.open(archive_path, "r:gz") as tar:
    members = tar.getmembers()
    contents = {member.name: tar.extractfile(member).read() if member.isfile() else None for member in members}
  return members, contents


python_members, python_contents = load(python_archive)
rust_members, rust_contents = load(args.rust_archive)

if args.plant_difference:
  planted = next(member.name for member in rust_members if member.isfile())
  rust_contents[planted] += b"planted difference\n"

FIELDS = ("name", "type", "mode", "uid", "gid", "uname", "gname", "mtime", "size", "linkname", "pax_headers")

diffs = []
python_names = [member.name for member in python_members]
rust_names = [member.name for member in rust_members]
if python_names != rust_names:
  diffs.append(f"member list differs:\n  python={python_names}\n  rust={rust_names}")
else:
  for py, rust in zip(python_members, rust_members):
    for field in FIELDS:
      py_value, rust_value = getattr(py, field), getattr(rust, field)
      if py_value != rust_value:
        diffs.append(f"{py.name!r}.{field}: python={py_value!r} rust={rust_value!r}")
    if python_contents[py.name] != rust_contents[rust.name]:
      diffs.append(f"{py.name!r}.content differs")

print(f"differential members: {len(python_names)} python, {len(rust_names)} rust; differences: {len(diffs)}")
for diff in diffs:
  print("DIFF", diff)
sys.exit(1 if diffs else 0)
