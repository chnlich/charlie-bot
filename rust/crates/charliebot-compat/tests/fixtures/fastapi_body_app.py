"""Small FastAPI app used by the Rust request-body differential test."""

import argparse
from typing import Literal

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


if __name__ == "__main__":
  parser = argparse.ArgumentParser()
  parser.add_argument("--port", type=int, required=True)
  arguments = parser.parse_args()
  import uvicorn

  uvicorn.run(app, host="127.0.0.1", port=arguments.port, log_level="error")
