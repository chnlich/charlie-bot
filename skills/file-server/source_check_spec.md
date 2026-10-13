## Goal

Check one page against its sources before the page is shared. Scope: delta.

The page is `{page}`. The draft hand-over message is `{message}`.

## Source Files

- {page}
- {message}

## Required Behavior

The declared terms are each cause, each mechanism, and each derived number that
the page or the message states. A derived number is a sum, a ratio, or a
difference that the page computes.

1. Read the page and the message.
2. List each declared term with the statement that carries it.
3. For each term, open the source that the page cites for the term.
4. Read cited code at the revision that the page names.
5. Check the statement of each term against its source.
6. Recompute each derived number from the values that the source gives.

## Acceptance Tests

- Each declared term has one line in the report.
- The line for a derived number carries the recomputed value.

## Reviewer Checklist

- The verify report is the final output of this task.

## Out of Scope

- Readability, wording, and layout of the page; the cold read judges them.
- A statement that reads a current value back from a system.
- Revisions of the page or the message; the manager makes them from the report.
