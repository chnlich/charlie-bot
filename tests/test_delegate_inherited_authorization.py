"""The runtime gate on the real v2 entry points.

The v2 policy is one central owner: the runtime gate
(``TaskTreeManager.check_task_authorization``), applied at the request
entries — delegation, improve, and agent messages to worker nodes — and never
re-judged at a Run's launch. A manager delegates through the real
/api/internal/delegate route when its open task chain holds a real user
message, a question included; a closed ancestor refuses; verify stays exempt
(read-only) on the route. Every scenario here runs the actual HTTP route
against a synthetic task tree with a scripted backend.
"""

from __future__ import annotations
