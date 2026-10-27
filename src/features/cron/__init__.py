from src.infra import metadata_slot_registration
from src.runtime.hooks import sequence_controllers, turn_contributions, wiring


def register() -> None:
  turn_contributions.register_turn_contribution("cron", "src.features.cron.turn_contribution:CONTRIBUTION")
  sequence_controllers.register_sequence_controller(
      "cron", "src.features.cron.sequence_controller:CronSequenceController")
  metadata_slot_registration.register_metadata_fields(
      "cron", "src.features.cron.metadata:CronMetadata", after="backend")
  wiring.register_setup_step("src.features.cron.seed", attr="setup_step")
