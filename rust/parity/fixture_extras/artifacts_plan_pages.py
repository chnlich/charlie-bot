"""Create the plan present and amend parity sessions and their plan pages.

The lineage session starts with no plans, so its present and amend steps build it in order. Its pages pass every plan
assertion. The refusal session holds the page that fails two assertions. The closed and two-open sessions hold the
registries that refuse an amend without a plan id, and the other session holds an artifact that the lineage session
names from outside its own directory.
"""

import pathlib
import re

from rust.parity.fixture import Builder

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
TITLES = ("Problem / Goal", "Context", "High Level Solution", "Detailed Design", "Trade-offs", "Other Details")
CREATED = "2026-07-20T00:00:00+00:00"


def plan_page(goal: str) -> str:
  """The genre template's style block verbatim, six numbered sections and a footer, with `goal` as the goal body."""
  template = (REPO_ROOT / "prompts" / "plan_template.html").read_text(encoding="utf-8")
  style = re.search(r"<style>.*?</style>", template, re.DOTALL).group(0)
  sections = "".join(
      f'<section class="plan-section"><h2><span class="n">{number}</span> {title}</h2>'
      f"<p>{goal if number == 1 else title}</p></section>" for number, title in enumerate(TITLES, 1))
  return f'<html><head>{style}</head><body>{sections}<div class="foot"><p>How to respond.</p></div></body></html>'


def _write_page(b: Builder, session_id: str, name: str, source: str) -> None:
  directory = b.cfg.sessions_dir / session_id / "artifacts"
  directory.mkdir(parents=True, exist_ok=True)
  (directory / name).write_text(source, encoding="utf-8")


def _awaiting(plan_id: int) -> dict:
  return {
      "id": plan_id,
      "title": f"Plan {plan_id}",
      "versions":
          [
              {
                  "v": 1,
                  "file": f"artifacts/plan_open_{plan_id}.html",
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
  lineage = await b.node("pages_lineage", "Plan page lineage")
  other = await b.node("pages_other", "Plan page other session")
  refusals = await b.node("pages_refusals", "Plan page refusals")
  closed = await b.node("pages_closed", "Plan page closed lineage")
  two_open = await b.node("pages_two_open", "Plan page two open lineages")

  good = plan_page("Ship the fix.")
  for name in ("plan_one.html", "plan_two.html", "plan_three.html", "plan_blank.html"):
    _write_page(b, lineage, name, good)
  _write_page(b, other, "plan_other.html", good)
  broken = good.replace('<span class="n">4</span>', '<span class="n">9</span>').replace(
      '<div class="foot"><p>How to respond.</p></div>', "")
  _write_page(b, refusals, "plan_broken.html", broken)
  for session in (closed, two_open):
    _write_page(b, session, "plan_extra.html", good)

  closed_plan = _awaiting(1) | {"closed": {"as": "completed", "at": CREATED}}
  await b.plans._save(closed, {"plans": [closed_plan]})
  await b.plans._save(two_open, {"plans": [_awaiting(1), _awaiting(2)]})
  return {}
