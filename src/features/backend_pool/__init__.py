"""The backend pool's metadata keys: a worker node records whether the pool chose its backend."""

from src.infra import metadata_slot_registration


def register() -> None:
  metadata_slot_registration.register_metadata_fields("backend_pool", "src.runtime.backend_pool:PoolNodeMetadata")
