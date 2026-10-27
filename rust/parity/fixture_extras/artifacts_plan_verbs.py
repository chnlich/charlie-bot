"""Create the plan-verb parity sessions: one with four awaiting plans for the approve and close refusals and each close mode,
one with a single awaiting plan for the bare approve and the listing flag, and one whose registry starts with a byte-order
mark, which the verbs' strict read refuses as invalid JSON.
"""

from rust.parity.fixture import Builder

CREATED = "2026-07-20T00:00:00+00:00"


def _awaiting(plan_id: int) -> dict:
  return {
      "id": plan_id,
      "title": f"Plan {plan_id}",
      "versions":
          [
              {
                  "v": 1,
                  "file": f"artifacts/plan_{plan_id:02d}.html",
                  "created_at": CREATED,
                  "trigger": "initial",
                  "base": None,
                  "note": None,
              }
          ],
      "takeoff": None,
      "closed": None,
  }


async def build(b: Builder) -> dict:
  lineages = await b.node("verbs_lineages", "Plan verb lineages")
  await b.plans._save(lineages, {"plans": [_awaiting(plan_id) for plan_id in (1, 2, 3, 4)]})

  single = await b.node("verbs_single", "Plan verb single lineage")
  await b.plans._save(single, {"plans": [_awaiting(1)]})

  corrupt = await b.node("verbs_corrupt", "Plan verb corrupt registry")
  (b.cfg.sessions_dir / corrupt / "plans.json").write_text('﻿{"plans": []}', encoding="utf-8")
  return {}
