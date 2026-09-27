"""Verify-result trailer format — the single authority for the trailer regex.

This module owns the verify-trailer surface ``src.core.spawner_prompt`` interpolates into the
verify prompt: the expected final ``RESULT:`` line a verify worker's report must end with. It
knows nothing about the plan registry; the dependency direction is spawner -> verify_trailer.
"""

import re

# ---------------------------------------------------------------------------
# Verify-result trailer — single authority
# ---------------------------------------------------------------------------

VERIFY_RESULT_TRAILER_RE = re.compile(r"RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \([0-9]+ approval\))")
VERIFY_RESULT_TRAILER_EXPECTED = f"`{VERIFY_RESULT_TRAILER_RE.pattern}`"
