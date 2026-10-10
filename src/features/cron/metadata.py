"""Metadata fields owned by the cron package."""

import pydantic

from src.infra import models


class CronMetadata(pydantic.BaseModel):
  scheduled_task: str | None = None
  last_scheduled_run: str | None = None
  last_run_status: models.LastRunStatus | None = None
  last_scheduled_cron: str | None = None
