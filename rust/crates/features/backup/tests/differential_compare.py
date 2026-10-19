"""Compares the Python and Rust backups of one home through Python's tarfile.

Usage: differential_compare.py <home> <rust_archive>. The home must hold exactly one Python
archive already; the driver creates the second and compares member lists, metadata and content.
"""

import os
import pathlib
import sys
import tarfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[5] / "src"))

home = pathlib.Path(sys.argv[1])
rust_archive = pathlib.Path(sys.argv[2])

os.environ["CHARLIEBOT_HOME"] = str(home)
from src.features.backup import backup  # noqa: E402

python_archive = backup.create_backup()


def load(archive_path):
    with tarfile.open(archive_path, "r:gz") as tar:
        members = tar.getmembers()
        contents = {
            member.name: tar.extractfile(member).read() if member.isfile() else None
            for member in members
        }
    return members, contents


python_members, python_contents = load(python_archive)
rust_members, rust_contents = load(rust_archive)

diffs = []
python_names = [member.name for member in python_members]
rust_names = [member.name for member in rust_members]
if python_names != rust_names:
    diffs.append(f"member list differs:\n  python={python_names}\n  rust={rust_names}")
else:
    for py, rust in zip(python_members, rust_members):
        for field in ("name", "type", "mode", "uid", "gid", "uname", "gname", "mtime", "size", "linkname"):
            py_value, rust_value = getattr(py, field), getattr(rust, field)
            if py_value != rust_value:
                diffs.append(f"{py.name}.{field}: python={py_value!r} rust={rust_value!r}")
        if python_contents[py.name] != rust_contents[rust.name]:
            diffs.append(f"{py.name}.content differs")

print(f"differential members: {len(python_names)} python, {len(rust_names)} rust; differences: {len(diffs)}")
for diff in diffs:
    print("DIFF", diff)
sys.exit(1 if diffs else 0)
