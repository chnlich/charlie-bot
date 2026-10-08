from src.infra import metadata_slots
from src.runtime.hooks import sequence_controllers, wiring


def register() -> None:
  sequence_controllers.register_sequence_controller("cron", "src.features.cron.sequence_controller:CronSequenceController")
  metadata_slots.register_metadata_fields("cron", "src.features.cron.metadata:CronMetadata", after="backend")
  wiring.register_router("src.features.cron.api", prefix="/api/cron", tags=("cron",))
  wiring.register_service("scheduler", "src.features.cron.service")
  wiring.register_startup_check("src.features.cron.backend_refs", attr="check_backend_refs")
