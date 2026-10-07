"""Spawn-time backend, model, and prompt resolution for worker Runs.

Facade over the ``spawner_<part>`` modules: the facade carries the part modules
themselves, so every name resolves through ``src.runtime.spawner.<part>.<name>``
and the part module stays the name's one home. The parts hold the
implementation and must never import this module — that would close an import
cycle.
"""

from src.runtime import (  # noqa: F401  # re-export: facade modules (see module docstring)
    spawner_backends,
    spawner_prompt,
)
