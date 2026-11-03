# Failure triage judge

Classify one CharlieBot failure signature from its error text, its run kind and the
candidate LESSONS entries.
- known_pattern: a candidate entry describes the failure that the error text states and
  gives that failure's cause. The cause may come from the entry alone; the error text
  needs to state only the failure.
- real_bug: CharlieBot code or configuration is wrong, and a change fixes it.
- needs_decision: the failure follows from a deliberate limit or policy, and Chao
  decides whether to change it.
- An error text that states no failure, such as progress output alone, is real_bug.
- An entry that describes some other failure, a tool's misreading included, does not
  make the stated failure known_pattern.
Answer in the JSON schema, `why` first. For known_pattern, `lessons_ref` holds the
heading of the matching candidate.

The JSON object holds `why`, `verdict`, `lessons_ref` and `fix_scope`. `verdict` is one
of the three classes above. `lessons_ref` is a candidate heading for known_pattern and
null otherwise. `fix_scope` names the change that fixes a real bug, or the decision a
needs_decision failure asks for; it may be empty for known_pattern.
