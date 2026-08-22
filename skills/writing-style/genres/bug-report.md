# Bug reports

Style for a fault report handed to whoever can fix it: another team, a vendor,
or an on-call rotation. The reader did not see the incident and owns a decision
at the end.

## Order

Bottom line first: what is broken, where, and its current containment state.
Then impact, quantified; then evidence, reproduction, and the ask. A reader
who stops after the first section still knows whether to act.

## Localization

Name the faulty part by identity: node, device index, serial, UUID, bus
address, firmware and driver versions. A repair or replacement request needs
these fields verbatim, so they belong in a table rather than in prose.

## Evidence

Carry one controlled comparison: the same test against a healthy peer in the
same environment, with the numbers side by side. A single failing measurement
proves a fault exists; the peer that passes proves where it lives.

State what the evidence rules out, with the counter values that rule it out.

When the evidence contains a signal the reader will recognize but that points
the wrong way (an error code that usually means something else), address it
head-on; a reader left alone with it re-derives the wrong conclusion.

Mark the unattempted step and the untested hypothesis as such, and name the
test that would settle each; the reader chooses a repair by that certainty.

## Reproduction

Every command runs as written: environment first, then the steps, then the
cleanup that removes what the steps created. Mark each command read-only or
mutating. Call out the option whose absence keeps the commands running but
changes what they measure.

## The ask

One decision, stated as a decision, with the option you recommend and its
cost. Separate it from the constraint that holds regardless of which option
wins, and name the routine action that would silently undo the containment.

## Sanitizing

Strip host paths, usernames, and credentials before the report leaves the
team. Internal node and service names stay while the reader operates that
infrastructure; they go when the destination is public.
