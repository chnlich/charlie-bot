"""Port reservation and listener ownership for the parity servers."""

import concurrent.futures
import os
import pathlib
import socket
import threading

import pytest

from rust.parity import servers

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]


def test_wait_ready_refuses_a_listener_outside_the_server_tree(tmp_path: pathlib.Path) -> None:
  # This test process holds the listener, so the server's tree never owns the port.
  with servers.pick_ports(1) as ports, socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    listener.bind((servers.HOST, ports[0]))
    listener.listen()
    tree = servers.make_tree("candidate", tmp_path / "candidate")
    server = servers.ServerProcess("candidate", ["/usr/bin/sleep", "60"], tree, ports[0], REPO_ROOT, 1)
    server.start()
    try:
      with pytest.raises(RuntimeError, match=f"port {ports[0]}") as error:
        server.wait_ready()
    finally:
      server.kill()
  assert str(os.getpid()) in str(error.value)


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
