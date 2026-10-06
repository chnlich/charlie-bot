"""Every `skills/<name>/<rest>` reference in prompts/ resolves inside the repo.

The prompts name genre and skill files at the rule that triggers writing (the
worker commit step, the master draft rule, the cron jobs). A renamed or moved
file leaves such a pointer dangling and the rule silently unwritten, so the
test walks every prompts/ file and resolves each reference against skills/.
"""

import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[1]
PROMPTS_DIR = ROOT / "prompts"
SKILLS_DIR = ROOT / "skills"

# A reference is `skills/<name>/<rest>`. The rest stops at the first character
# a repo path cannot contain (whitespace, quote, paren, backtick), so the
# markdown punctuation around a reference never joins the path; a trailing
# sentence period rides inside the rest and is stripped below.
_REFERENCE = re.compile(r"skills/(?P<name>[A-Za-z0-9_-]+)/(?P<rest>[A-Za-z0-9_./-]+)")
# Sentence punctuation a reference may pick up: `)`, `.`, `,`, backtick, quotes.
_TRAILING_PUNCTUATION = ").,`\"'"


def test_prompt_skill_references_resolve() -> None:
  skill_names = {path.name for path in SKILLS_DIR.iterdir() if path.is_dir()}
  broken = []
  checked = 0
  for path in sorted(PROMPTS_DIR.rglob("*")):
    if not path.is_file():
      continue
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
      for match in _REFERENCE.finditer(line):
        name = match.group("name")
        if name not in skill_names:
          continue
        checked += 1
        rest = match.group("rest").rstrip(_TRAILING_PUNCTUATION)
        target = SKILLS_DIR / name / rest
        if not target.exists():
          broken.append(
              f"{path.relative_to(ROOT)}:{lineno}: {match.group(0)} -> {target.relative_to(ROOT)} does not exist")

  assert checked > 0, "no skills/<name>/<rest> reference found under prompts/ - the scan matched nothing"
  assert not broken, "prompts/ reference(s) under skills/ do not resolve in the repo:\n  " + "\n  ".join(broken)
