"""Backend-switch endpoint: the continuation-domain rule and the API contract.

Carries two of the acceptance-tested mechanisms:
  * domain ⟷ reachability — ``same_continuation_domain`` is exactly the
    condition under which the runtime resume resolver can re-find a cc-claude
    target's transcript (a cross-family target is its own domain and starts
    its own conversation, which the endpoint accepts for an ordinary session);
  * the API contract over the four configured rows plus the idempotent no-op:
    an ordinary session switches in place across families, a cron-dedicated
    session keeps the in-domain rule, and the pre-rule backfill.
"""

# ---------------------------------------------------------------------------
# Acceptance #3: §4.1 API contract
# ---------------------------------------------------------------------------
