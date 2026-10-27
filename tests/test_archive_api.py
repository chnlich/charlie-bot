"""The archive API surface: POST archives the subtree, the message routes
enforce the input table (the user's message restores; an agent's is 409 with
the one archived sentence), and unarchive restores the chain — all through the
real TaskTreeManager and session blocks behind the real routers."""

from __future__ import annotations
