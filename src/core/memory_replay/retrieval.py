"""Feedback-example retrieval: a simple, inspectable selection over the supplied pool.

Selection scores each feedback example against one theme's frozen context on
two axes, so a correction survives a project rename:

- **principles**: exact tag match between the example's tags and the theme's
  declared principles — the question-level axis ("instance names out, mechanism
  kept") that does not mention any project name;
- **terms**: word overlap between the example's texts and the theme's candidate,
  entry, and document texts — the topic axis.

Tags are a rebuildable index over the user's own comments and approved texts,
never user rules: the original comment text and provenance always travel
intact, and the selection record shows what matched.
"""

import re
from dataclasses import dataclass

from src.core.memory_replay.manifest import FeedbackExample

# Words too generic to count as evidence of relevance. Tokens shorter than four
# characters are dropped outright; this list removes the rest of the noise.
_STOPWORDS = frozenset(
    {
        "that",
        "this",
        "with",
        "from",
        "have",
        "will",
        "your",
        "when",
        "what",
        "been",
        "they",
        "them",
        "than",
        "then",
        "into",
        "also",
        "only",
        "over",
        "such",
        "must",
        "should",
        "would",
        "could",
        "there",
        "their",
        "about",
        "which",
        "these",
        "those",
        "because",
        "while",
        "where",
        "after",
        "before",
        "every",
        "here",
        "some",
        "more",
        "very",
        "just",
        "like",
        "each",
        "both",
        "entry",
        "entries",
        "memory",
        "store",
    })

_TOKEN_RE = re.compile(r"[a-z0-9]+")


@dataclass
class FeedbackSelection:
  example: FeedbackExample
  score: int
  matched_principles: list[str]
  matched_terms: list[str]


def tokens(text: str) -> set[str]:
  """Lowercase word tokens of length >= 4 minus the stopword set."""
  return {t for t in _TOKEN_RE.findall(text.lower()) if len(t) >= 4 and t not in _STOPWORDS}


def select_feedback(
    examples: list[FeedbackExample],
    *,
    principles: set[str],
    context_text: str,
) -> list[FeedbackSelection]:
  """Return the examples relevant to one theme, most relevant first.

  An example is selected when its score is positive: two points per matched
  principle plus one point per distinct matched term. Ties break on
  comment_event, so the selection is deterministic for identical inputs.
  """
  context_terms = tokens(context_text)
  selections: list[FeedbackSelection] = []
  for example in examples:
    matched_principles = sorted(set(example.tags) & principles)
    example_terms = tokens(_example_text(example))
    matched_terms = sorted(example_terms & context_terms)
    score = 2 * len(matched_principles) + len(matched_terms)
    if score > 0:
      selections.append(
          FeedbackSelection(
              example=example, score=score, matched_principles=matched_principles, matched_terms=matched_terms))
  return sorted(selections, key=lambda s: (-s.score, s.example.comment_event))


def _example_text(example: FeedbackExample) -> str:
  parts = [example.comment_text]
  if example.approved_change is not None:
    parts.append(example.approved_change.before)
    parts.append(example.approved_change.after)
  return "\n".join(parts)
