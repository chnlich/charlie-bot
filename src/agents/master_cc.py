"""Master CC — spawns a Claude Code subprocess for the master agent.

Facade over the ``master_cc_<part>`` modules: the facade carries the part modules
themselves, so every name resolves through ``src.agents.master_cc.<part>.<name>``
and the part module stays the name's one home. The parts hold the
implementation and must never import this module — that would close an import
cycle.
"""

from src.agents import (  # noqa: F401  # re-export: facade modules (see module docstring)
    master_cc_queue,
    master_cc_run,
    master_cc_state,
)
