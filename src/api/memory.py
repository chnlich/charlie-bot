"""Memory management API routes."""

from fastapi import APIRouter, Depends

from src.api.deps import get_memory
from src.core.memory import MemoryManager

router = APIRouter()


@router.get("/memory")
async def get_memory_content(memory: MemoryManager = Depends(get_memory)):
  content = await memory.read_memory()
  return {"content": content}


@router.get("/progress")
async def get_progress(memory: MemoryManager = Depends(get_memory)):
  content = await memory.read_progress()
  return {"content": content}
