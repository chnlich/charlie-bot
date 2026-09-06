"""Diagnostics API routes — write-only session-switch telemetry points."""

from typing import Literal

import structlog
from fastapi import APIRouter
from pydantic import BaseModel, ConfigDict, Field

log = structlog.get_logger()

router = APIRouter()

MAX_ERROR_CHARS = 300


class SwitchEventRequest(BaseModel):
  """One session-switch telemetry point; unknown fields mean a stale client, reject them."""

  model_config = ConfigDict(extra='forbid')

  phase: Literal['started', 'completed', 'superseded', 'failed', 'render_error']
  from_session: str | None
  to_session: str
  generation: int
  winner_generation: int | None
  elapsed_ms: int | None = Field(default=None, ge=0)
  error: str | None
  client_ts: str


@router.post('/switch-events')
async def post_switch_event(req: SwitchEventRequest):
  """Log one switch-telemetry point as a single diag_switch line; no persistence."""
  record = req.model_dump()
  error = record['error']
  if error is not None and len(error) > MAX_ERROR_CHARS:
    record['error'] = error[:MAX_ERROR_CHARS]
  log.info('diag_switch', **record)
  return {'ok': True}
