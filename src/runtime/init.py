"""Initialize ~/.charliebot/ directory structure on first run.

Facade over the home seeding module. The part holds the implementation and
must never import this module — that would close an import cycle.
"""

from src.runtime import init_seed  # noqa: F401
