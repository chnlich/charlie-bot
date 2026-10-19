"""A quiet worker whose plan registry names one artifact inside its session directory and one in another session.

The fork copy takes the inside artifact along, and names the outside one by a fallback path inside the child.
"""

from rust.parity.fixture import Builder

CREATED = "2026-07-20T00:00:00+00:00"


async def build(b: Builder) -> dict[str, str]:
  await b.node("plan_fork_root", "Synthetic plan fork root")
  worker = await b.node("plan_fork_worker", "Synthetic plan fork worker", profile="worker", parent="plan_fork_root")
  other = await b.node("plan_fork_other", "Synthetic plan fork other session")

  inside = b.cfg.sessions_dir / worker / "artifacts"
  inside.mkdir(parents=True, exist_ok=True)
  (inside / "plan_inside.html").write_text("<p>Synthetic inside plan</p>\n", encoding="utf-8")
  outside = b.cfg.sessions_dir / other / "artifacts"
  outside.mkdir(parents=True, exist_ok=True)
  (outside / "plan_outside.html").write_text("<p>Synthetic outside plan</p>\n", encoding="utf-8")

  await b.plans._save(
      worker, {
          "plans":
              [
                  {
                      "id": 1,
                      "title": "Forked plan",
                      "versions":
                          [
                              {
                                  "v": 1,
                                  "file": "artifacts/plan_inside.html",
                                  "created_at": CREATED,
                                  "trigger": "initial",
                                  "base": None,
                                  "note": None,
                              },
                              {
                                  "v": 2,
                                  "file": f"../{other}/artifacts/plan_outside.html",
                                  "created_at": CREATED,
                                  "trigger": "feedback",
                                  "base": None,
                                  "note": "Synthetic revision",
                              },
                          ],
                      "takeoff": {
                          "v": 1,
                          "at": CREATED
                      },
                      "closed": None,
                  }
              ]
      })
  return {}
