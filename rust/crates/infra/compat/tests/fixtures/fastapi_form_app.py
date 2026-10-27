"""Small FastAPI app used by the Rust multipart differential test."""

import argparse

import fastapi
from fastapi import File, Form, UploadFile

app = fastapi.FastAPI()


async def require_item(item_id: str) -> str:
  if item_id == "missing":
    raise fastapi.HTTPException(status_code=404, detail="missing item")
  return item_id


@app.post("/items/{item_id}")
async def read_item(
    file: UploadFile = File(...),
    note: str = Form(...),
    item_id: str = fastapi.Depends(require_item),
) -> dict:
  content = await file.read()
  return {"filename": file.filename, "size": len(content), "note": note}


if __name__ == "__main__":
  parser = argparse.ArgumentParser()
  parser.add_argument("--port", type=int, required=True)
  arguments = parser.parse_args()
  import uvicorn

  uvicorn.run(app, host="127.0.0.1", port=arguments.port, log_level="error")
