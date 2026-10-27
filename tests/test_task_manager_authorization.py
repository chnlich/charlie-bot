"""Logical-manager authorization boundary on the real entry points.

The user's correction: a logical manager session is agent-organizable — a valid
active manager Run creates logical manager children under its own open task
through the ordinary task-create API/CLI at any depth, and archives its own
task through the completion owner, both with no user
message anywhere on the tree. Implementation stays gated: worker children ride
the runtime gate (a real user message or a thread summon on the open task
chain), agent relay text and child reports never count as user requests, and
caller scope (own parent, manager parent, open ancestry, replay identity) is
enforced exactly as before.
Every scenario here runs the actual HTTP routes against a synthetic instance
with scripted backends; no real model or provider takes part.
"""

from __future__ import annotations

# ---------------------------------------------------------------------------
# Creation without takeoff: durable, stable, any depth
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Normal own-run archive without authorization prompts
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Implementation stays gated on the same tree
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Caller scope, token identity, replay identity
# ---------------------------------------------------------------------------
