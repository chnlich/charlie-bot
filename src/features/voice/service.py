"""The speech service: provisions the speech models in the background and warms the decode path."""

import asyncio
import time

from src.infra import config, log_once, tasks
from src.runtime.hooks import wiring

log = log_once.LazyStructlogLogger()

_provisioning_task: asyncio.Task | None = None


def _provision_speech_models(cfg: config.CharlieBotConfig) -> None:
  """Provision the speech models on a worker thread, then warm the decode path.

  src.features.voice.transcriber carries the numpy import (~90 ms), so the module loads
  here instead of the event loop's startup path: the M99 import floor
  (docs/perf_baseline.md@5175adf09) prices the import's wall, and this thread's span is
  exactly the cost the metric does not see.

  After provisioning opens readiness, the same thread builds the resident bundle
  (single-flight, so a concurrent first request shares it) and decodes one
  synthetic sine, moving the ~12 s one-time cold cost off the first request. A
  warm failure only logs: readiness stays exactly as provisioning published it
  and the endpoints keep their lazy path as the fallback.
  """
  from src.features.voice import transcriber  # deferred: server start

  transcriber.provision_models(cfg)
  started = time.monotonic()
  try:
    bundle = transcriber.get_transcription_bundle(cfg)
    transcriber.warm_up_bundle(bundle)
  except Exception:
    # Includes the not-ready raise of a parked provisioning failure: log it loudly
    # and keep booting — a warm failure never parks readiness nor kills the thread.
    log.exception("voice_warmup_failed")
    return
  log.info("voice_warmup_complete", elapsed_ms=round((time.monotonic() - started) * 1000))


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Start provisioning on a worker thread; the server does not wait for it."""
  global _provisioning_task
  _provisioning_task = tasks.create_logged_task(
      asyncio.to_thread(_provision_speech_models, ctx.cfg), name="speech-model-provisioning")


async def stop_service() -> None:
  """Cancel provisioning; returns at once when start_service never ran."""
  global _provisioning_task
  task, _provisioning_task = _provisioning_task, None
  await tasks.cancel_and_wait(task)
