"""Write the fork-copy fixtures that the artifacts crate's sidebar test compares with.

The parent registry names a version inside the session, a version under a path the registry keeps as a
suffix, one artifact that is missing, and one artifact outside the parent session. The child registry is the
bytes that ``_copy_plans_to_child`` writes for it. Run from the worktree root:
``uv run python rust/crates/features/artifacts/tests/plans_fork_fixture.py``.
"""

import json
import pathlib
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))

from src.features.artifacts import sidebar  # noqa: E402

DATA = pathlib.Path(__file__).resolve().parent / "data"

CREATED = "2026-07-20T00:00:00+00:00"


def _version(v: int, file: str | None, trigger: str = "initial", note: str | None = None) -> dict:
  return {"v": v, "file": file, "created_at": CREATED, "trigger": trigger, "base": None, "note": note}


PARENT_REGISTRY = {
    "plans":
        [
            {
                "id": 1,
                "title": "Synthetic plan",
                "versions":
                    [
                        _version(1, "artifacts/collision.html"),
                        _version(2, "./artifacts/plan_02.html", "feedback", "Split the executor."),
                        _version(3, "../other/artifacts/collision.html", "feedback", "Pulled from another session."),
                    ],
                "takeoff": None,
                "closed": None,
            },
            {
                "id": 2,
                "title": "Approved plan",
                "versions": [_version(1, None), _version(2, "artifacts/plan_03.html")],
                "takeoff": {
                    "v": 2,
                    "at": CREATED
                },
                "closed": None,
            },
        ],
    "registry_note": "Unknown keys ride along on the copy.",
}


def main() -> None:
  parent_text = json.dumps(PARENT_REGISTRY, indent=2, ensure_ascii=False) + "\n"
  with tempfile.TemporaryDirectory() as tmp:
    sessions = pathlib.Path(tmp) / "sessions"
    parent = sessions / "parent"
    child = sessions / "child"
    (parent / "artifacts").mkdir(parents=True)
    (sessions / "other" / "artifacts").mkdir(parents=True)
    child.mkdir(parents=True)
    (parent / "artifacts" / "collision.html").write_text("<html>parent</html>", encoding="utf-8")
    (parent / "artifacts" / "plan_02.html").write_text("<html>plan 2</html>", encoding="utf-8")
    (sessions / "other" / "artifacts" / "collision.html").write_text("<html>other</html>", encoding="utf-8")
    (parent / "plans.json").write_text(parent_text, encoding="utf-8")
    sidebar._copy_plans_to_child(parent, child)
    DATA.mkdir(exist_ok=True)
    (DATA / "fork_parent_plans.json").write_text(parent_text, encoding="utf-8")
    (DATA / "fork_child_plans.json").write_bytes((child / "plans.json").read_bytes())


if __name__ == "__main__":
  main()
