import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
  sys.path.insert(0, str(ROOT))


class JudgmentShim:
  """Default finalize-judgment reads for test fakes: no prior effects recorded.

  The finalize chain gates its side effects on judgment reads
  (src/core/finalize_effects): chat events for the summary/master-wake checks,
  thread lists for the reviewer-exists check. Fakes that predate those gates
  inherit "nothing recorded yet" from here so their tests keep exercising the
  always-persist / always-trigger / always-spawn behavior.
  """

  def load_chat_events_sync(self, session_id: str) -> list[dict[str, Any]]:
    return []

  async def list_threads(self, session_id: str) -> list[Any]:
    return []
