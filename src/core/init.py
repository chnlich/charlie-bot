"""Initialize ~/.charliebot/ directory structure on first run.

Facade over the ``init_<part>`` modules: the facade carries the part modules
themselves, so every name resolves through ``src.core.init.<part>.<name>``
and the part module stays the name's one home. The parts hold the
implementation and must never import this module — that would close an import
cycle.
"""

from src.core import (  # noqa: F401  # re-export: facade modules (see module docstring)
    init_master_recovery,
    init_seed,
    init_worker_recovery,
)
