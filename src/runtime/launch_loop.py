"""The launch loop shared by master turns and task runs.

A run is a loop of processes. The backend lifecycle (src/runtime/hooks/backend_lifecycle.py)
places the first process, watches each process, plans relays, and cleans up after the round. A
backend without a login pool runs one process. The master turn (src/runtime/master_cc_run.py) and
task run (src/runtime/worker.py) supply only the code that builds and runs one process.
"""

from collections.abc import Awaitable, Callable, Mapping

from src.infra import constants
from src.runtime.hooks import backend_lifecycle

# (launch, watch, relays before this process) -> (exit code, stderr text)
RunProcess = Callable[[backend_lifecycle.Launch, backend_lifecycle.LaunchWatch | None, int], Awaitable[tuple[int, str]]]
OnRelay = Callable[[int], None]


async def run_launches(
    ctx: backend_lifecycle.LaunchContext,
    lifecycle: backend_lifecycle.BackendLifecycle,
    *,
    run_process: RunProcess,
    native_id: Callable[[], str | None],
    on_relay: OnRelay,
) -> int:
  """Place and run one backend lifecycle; return the number of relays.

  run_process builds the backend of one launch, streams its events, stops the process when
  watch.observe asks for it, and returns the exit code and stderr text. relays counts processes
  that ran before this one. native_id reads the conversation id as the processes reported it. A
  refused placement propagates before the round starts; a refused relay propagates after
  unsuccessful after_round cleanup.
  """
  launch = await lifecycle.place(ctx)
  relays = 0
  exit_code = 1
  completed = False
  try:
    while True:
      watch = lifecycle.watch(ctx, launch)
      exit_code, stderr = await run_process(launch, watch, relays)
      if watch is None or not watch.wants_next(exit_code, stderr):
        completed = True
        break
      next_launch = await lifecycle.next_launch(ctx, launch, native_id())
      relays += 1
      on_relay(relays)
      launch = next_launch
  finally:
    await lifecycle.after_round(ctx, native_id(), succeeded=completed and exit_code == 0)
  return relays


def child_env(inherited: Mapping[str, str]) -> dict[str, str]:
  """Copy an inherited environment, strip its session identity, and apply package edits."""
  env = dict(inherited)
  env.pop(constants.SESSION_ID_ENV_VAR, None)
  backend_lifecycle.apply_child_env(env)
  return env


def has_round_notices(lifecycle: backend_lifecycle.BackendLifecycle) -> bool:
  """True when the lifecycle overrides round_notices and reads a round's projected events."""
  return type(lifecycle).round_notices is not backend_lifecycle.BackendLifecycle.round_notices
