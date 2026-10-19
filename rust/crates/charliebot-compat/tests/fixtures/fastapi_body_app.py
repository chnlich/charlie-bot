"""Small FastAPI app used by the Rust request-body differential test."""

import argparse
from typing import Annotated, Literal

import fastapi
import pydantic

app = fastapi.FastAPI()


class RequestModel(pydantic.BaseModel):
  name: str
  note: str | None
  count: int | None
  enabled: bool
  mode: Literal["fast", "safe"]
  labels: list[str]


class BoundedRequest(pydantic.BaseModel):
  model_config = pydantic.ConfigDict(extra="forbid")

  offset: int | None = pydantic.Field(default=None, ge=0)
  count: int = pydantic.Field(ge=1, le=100)
  ratio: float | None = pydantic.Field(default=None, gt=0.0, le=1.0)
  label: str = pydantic.Field(min_length=1)
  tags: list[str] = pydantic.Field(max_length=2)


class UploadedFileRef(pydantic.BaseModel):
  filename: str
  path: str
  size: int | None = None


class SendMessageRequest(pydantic.BaseModel):
  content: str
  uploaded_files: list[UploadedFileRef] = pydantic.Field(default_factory=list)
  input_mode: str | None = None


class StrictFileRef(pydantic.BaseModel):
  model_config = pydantic.ConfigDict(extra="forbid")

  filename: str
  size: int | None = None


class AttachmentsRequest(pydantic.BaseModel):
  attachments: list[StrictFileRef] | None = pydantic.Field(default=None, min_length=2, max_length=3)


class LocalPid(pydantic.BaseModel):
  kind: Literal["local_pid"] = "local_pid"
  pid: int
  exit_file: str | None = None
  log: str | None = None


class RemotePid(pydantic.BaseModel):
  kind: Literal["remote_pid"] = "remote_pid"
  host: str
  pid: int
  exit_file: str | None = None
  log: str | None = None


class SlurmJob(pydantic.BaseModel):
  kind: Literal["slurm_job"] = "slurm_job"
  host: str | None = None
  job_id: int


WatchTarget = Annotated[LocalPid | RemotePid | SlurmJob, pydantic.Field(discriminator="kind")]


class ScheduleTriggerRequest(pydantic.BaseModel):
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  delay_seconds: int
  message: str = pydantic.Field(max_length=200)
  watch_targets: list[WatchTarget] | None = None


async def require_item(item_id: str) -> str:
  if item_id == "missing":
    raise fastapi.HTTPException(status_code=404, detail="missing item")
  return item_id


@app.post("/items/{item_id}")
async def read_item(body: RequestModel, item_id: str = fastapi.Depends(require_item)) -> dict:
  return body.model_dump(mode="json")


@app.post("/bounds/{item_id}")
async def read_bounded(body: BoundedRequest, item_id: str = fastapi.Depends(require_item)) -> dict:
  return body.model_dump(mode="json")


# The list routes echo only the keys the body sent, as the Rust validated object holds them.
@app.post("/messages/{item_id}")
async def read_message(body: SendMessageRequest, item_id: str = fastapi.Depends(require_item)) -> dict:
  return body.model_dump(mode="json", exclude_unset=True)


@app.post("/attachments/{item_id}")
async def read_attachments(body: AttachmentsRequest, item_id: str = fastapi.Depends(require_item)) -> dict:
  return body.model_dump(mode="json", exclude_unset=True)


@app.post("/triggers/{item_id}")
async def read_trigger(body: ScheduleTriggerRequest, item_id: str = fastapi.Depends(require_item)) -> dict:
  return body.model_dump(mode="json", exclude_unset=True)


if __name__ == "__main__":
  parser = argparse.ArgumentParser()
  parser.add_argument("--port", type=int, required=True)
  arguments = parser.parse_args()
  import uvicorn

  uvicorn.run(app, host="127.0.0.1", port=arguments.port, log_level="error")
