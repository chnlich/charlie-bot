"""Offline memory-curation replay: frozen evidence in, complete-entry proposal out.

This package owns the whole replay pipeline (manifest, retrieval, model
exchange, mechanical validation, proposal bundle), the paired comparison of a
recorded run, and the fixed-input variant experiment
(``variants.py`` + ``experiment.py``); ``src/cli/memory.py`` is its thin
entry point. Nothing here touches the live memory store: current entries,
the topics vocabulary, and every piece of evidence arrive frozen in the
manifest, and the only writes go to the requested output root.
"""

from src.core.memory_replay.compare import CompareOptions, run_comparison
from src.core.memory_replay.errors import ReplayError
from src.core.memory_replay.experiment import ExperimentOptions, ExperimentOutcome, run_experiment
from src.core.memory_replay.runner import MODES, ReplayOptions, run_replay

__all__ = [
    "MODES",
    "CompareOptions",
    "ExperimentOptions",
    "ExperimentOutcome",
    "ReplayError",
    "ReplayOptions",
    "run_comparison",
    "run_experiment",
    "run_replay",
]
