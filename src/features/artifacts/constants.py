"""Vocabularies that the artifact CLI, the plan CLI and their validators share.

stdlib-only by contract: each CLI parses its arguments with these tuples and must not import pydantic
or the assertion machinery to do so.
"""

# Plan-registry verb vocabularies: the CLI's argparse choices (plan_cli) and the
# registry verbs' validation (plans) share one tuple per vocabulary, so the
# plan chain imports no pydantic to parse args. The request models' Literal types
# (src.infra.models PlanAmendTrigger / PlanCloseMode) are the type home; a tuple
# here and its Literal there list the same values. The named close-mode spellings are the
# home for the values plans derives and compares against (plans _derive_state,
# _DERIVED_STATE_STR): a closed plan's derived state IS its close mode's spelling.
PLAN_AMEND_TRIGGERS = ("auto_amend", "feedback")
PLAN_CLOSE_SUPERSEDED = "superseded"
PLAN_CLOSE_ABANDONED = "abandoned"
PLAN_CLOSE_COMPLETED = "completed"
PLAN_CLOSE_MODES = (PLAN_CLOSE_SUPERSEDED, PLAN_CLOSE_ABANDONED, PLAN_CLOSE_COMPLETED)

# Artifact genre vocabulary: the artifact CLI's argparse choices (cli) and
# the assertion registry (artifact_check _ASSERTION_SETS) share one tuple, so
# the artifact chain parses args without loading the assertion machinery (the M102 wrap
# wall). The registry is the home of what a genre means: adding a genre means registering
# its assertion set there AND naming it here; artifact_check's import-time equality check
# makes a missed step fail loud.
ARTIFACT_GENRES = ("plan", "understanding", "sitrep", "debug", "explain")
