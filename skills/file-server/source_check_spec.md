## Goal

Check one page against its sources. Scope: delta.

The page is `{page}`. The draft hand-over message is `{message}`.

## Source Files

- {page}
- {message}

## Required Behavior

The declared terms are the facts the page or draft hand-over message states:
- each cause;
- each mechanism;
- each derived number, a sum, ratio, or difference computed by the page;
- each current system value, a count, a time, or a status that a system holds at check time.

The check verifies each cited source location, recomputes each derived number from its cited source, and reads each
current system value back from the system that holds it.

## Acceptance Tests

- Each declared term has one line in the report.
- The line for a derived number carries the recomputed value.
- The line for a current system value carries the value read back and the system it came from.

## Reviewer Checklist

- The fact-check report is the final output of this task.

## Out of Scope

- Readability, wording, and layout of the page; the cold read judges them.
- Revisions of the page or the message; the page author makes them from the report.
