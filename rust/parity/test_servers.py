"""Port reservation and listener ownership for the parity servers."""

import concurrent.futures
import threading

from rust.parity import servers


def test_concurrent_pick_ports_return_disjoint_ports() -> None:
  barrier = threading.Barrier(2, timeout=10)

  def hold_two_ports() -> list[int]:
    # Each call keeps its reservation until both have picked, so the second pick runs against the first's locks.
    with servers.pick_ports(2) as ports:
      barrier.wait()
      return ports

  with concurrent.futures.ThreadPoolExecutor(2) as pool:
    futures = [pool.submit(hold_two_ports) for _ in range(2)]
    first, second = (future.result() for future in futures)
  assert len(first) == len(second) == 2
  assert not set(first) & set(second)
