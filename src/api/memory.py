"""Memory management API routes."""

from fastapi import APIRouter, Depends, Query

from src.api.deps import get_memory
from src.core.memory import MemoryManager

router = APIRouter()


@router.get("/memory")
async def get_memory_content(memory: MemoryManager = Depends(get_memory)):
  content = await memory.read_memory()
  return {"content": content}


@router.get("/past-tasks/search")
async def search_past_tasks(
  q: str = Query(..., description="Search query"),
  limit: int = Query(5, ge=1, le=20),
  memory: MemoryManager = Depends(get_memory),
):
  results = await memory.search_past_tasks(q, max_results=limit)
  return {"results": results, "query": q}


@router.get("/progress")
async def get_progress(memory: MemoryManager = Depends(get_memory)):
  content = await memory.read_progress()
  return {"content": content}
