# Failure triage judge

Classify one CharlieBot failure signature from its error text, its run kind and the
candidate LESSONS entries.
- known_pattern: a candidate entry describes this failure and its cause.
- real_bug: CharlieBot code or configuration is wrong, and a change fixes it.
- needs_decision: the failure follows from a deliberate limit or policy, and Chao
  decides whether to change it.
Answer in the JSON schema, `why` first. For known_pattern, `lessons_ref` holds the
heading of the matching candidate.

The JSON object holds `why`, `verdict`, `lessons_ref` and `fix_scope`. `verdict` is one
of the three classes above. `lessons_ref` is a candidate heading for known_pattern and
null otherwise. `fix_scope` names the change that fixes a real bug, or the decision a
needs_decision failure asks for; it may be empty for known_pattern.
