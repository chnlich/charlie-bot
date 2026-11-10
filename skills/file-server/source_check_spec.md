## Goal

Check one page against its sources before the page is shared. Scope: delta.

The page is `{page}`. The draft hand-over message is `{message}`.

## Source Files

- {page}
- {message}

## Required Behavior

The declared terms are each cause, mechanism, and derived number stated by the page or draft hand-over message. A
derived number is a sum, ratio, or difference computed by the page.

The check verifies each cited source location and recomputes each derived number from its cited source.

## Acceptance Tests

- Each declared term has one line in the report.
- The line for a derived number carries the recomputed value.

## Reviewer Checklist

- The verify report is the final output of this task.

## Out of Scope

- Readability, wording, and layout of the page; the cold read judges them.
- A statement that reads a current value back from a system.
- Revisions of the page or the message; the manager makes them from the report.
