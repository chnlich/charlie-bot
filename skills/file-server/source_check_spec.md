## Goal

Check one page against its sources. Scope: delta.

The page is `{page}`. The draft hand-over message is `{message}`.

## Source Files

- {page}
- {message}

## Required Behavior

The declared terms are every factual claim the page or draft hand-over message states, including:
- each cause;
- each mechanism;
- each derived number, a sum, ratio, or difference computed by the page;
- each current system value, a count, a time, or a status that a system holds at check time.

The check verifies each claim against its evidence, verifies each cited source location, recomputes each derived number
from its cited source, and reads each current system value back from the system that holds it.

Check each premise of every proposed option and acceptance criterion:
- no existing path, or a lighter one, already serves its need;
- the case it handles occurs in the current system;
- no earlier user decision of the line rejects it;
- it fits the known limits of context length, compute, and token cost.

## Acceptance Tests

- Each declared term has one line in the report.
- Each claim's line names its evidence and says whether the evidence supports it; when no supporting evidence is found,
  the line says so.
- The line for a derived number carries the recomputed value.
- The line for a current system value carries the value read back and the system it came from.
- Each premise has one line naming the code, config, or decision record read and saying whether the premise holds.

## Reviewer Checklist

- The fact-check report is the final output of this task.

## Out of Scope

- Readability, wording, and layout of the page; the cold read judges them.
- Changes to the page, the message, or any system; the fact-check worker returns findings for the page author to apply.
