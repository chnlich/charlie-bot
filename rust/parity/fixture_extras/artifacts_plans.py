"""Create the plan-registry parity sessions: one per corrupt registry shape, and one with awaiting, approved and closed plans.

The corrupt registries are written as raw text, the way a damaged file reaches the readers. The good registry goes
through the plan registry's verbs, as the fable fixture does.
"""

from rust.parity.fixture import Builder

CREATED = "2026-07-20T00:00:00+00:00"

# Each corrupt shape the tolerant read turns into error entries, keyed by its session. The invalid-JSON shape is
# not here: the harness parses every .json file in both homes, so a file that is not JSON stops the run. A BOM before
# a registry fails the same JSON decode in the read, and the harness's byte parser still reads it.
CORRUPT_REGISTRIES = {
    "plans_bom_prefix":
        '\ufeff{"plans": []}',
    "plans_top_list":
        '[{"id": 1, "versions": []}]',
    "plans_as_map":
        '{"plans": {"1": {"id": 1, "versions": []}}}',
    "plans_as_string":
        '{"plans": "awaiting approval"}',
    "plan_not_map":
        '{"plans": ["awaiting approval"]}',
    "plan_empty_versions":
        ('{"plans": [{"id": 1, "title": "No versions", "versions": [], "takeoff": null, '
         '"closed": null}]}'),
    "version_not_map":
        '{"plans": [{"id": 1, "title": "Bad version", "versions": ["artifacts/plan_01.html"]}]}',
    "closed_unknown_as":
        (
            '{"plans": [{"id": 1, "title": "Retired", "versions": [{"v": 1, '
            '"file": "artifacts/plan_01.html"}], "takeoff": null, "closed": {"as": "retired"}}]}'),
    "takeoff_as_string":
        (
            '{"plans": [{"id": 1, "title": "Approved as text", "versions": [{"v": 1, '
            '"file": "artifacts/plan_01.html"}], "takeoff": "approved"}]}'),
}


def _version(v: int, file: str) -> dict:
  return {"v": v, "file": file, "created_at": CREATED, "trigger": "initial", "base": None, "note": None}


async def build(b: Builder) -> dict:
  for key, text in CORRUPT_REGISTRIES.items():
    session = await b.node(key, f"Plan registry {key}")
    (b.cfg.sessions_dir / session / "plans.json").write_text(text, encoding="utf-8")

  lineages = await b.node("plans_lineages", "Plan lineages")
  await b.plans._save(
      lineages, {
          "plans":
              [
                  {
                      "id": 1,
                      "title": "Awaiting plan",
                      "versions": [_version(1, "artifacts/plan_awaiting.html")],
                      "takeoff": None,
                      "closed": None,
                  },
                  {
                      "id": 2,
                      "title": "Approved plan",
                      "versions": [_version(1, "artifacts/plan_approved.html")],
                      "takeoff": None,
                      "closed": None,
                  },
                  {
                      "id": 3,
                      "title": "Closed plan",
                      "versions": [_version(1, "artifacts/plan_closed.html")],
                      "takeoff": None,
                      "closed": None,
                  },
              ]
      })
  await b.plans.approve(lineages, 2)
  await b.plans.close(lineages, 3, "completed")
  return {}
