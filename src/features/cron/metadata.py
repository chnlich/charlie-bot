"""Metadata fields owned by the cron package."""

from pydantic import BaseModel

from src.infra.models import LastRunStatus


class CronMetadata(BaseModel):
  scheduled_task: str | None = None
  last_scheduled_run: str | None = None
  last_run_status: LastRunStatus | None = None
  last_scheduled_cron: str | None = None
