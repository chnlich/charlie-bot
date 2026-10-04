"""Package pin for process-level environment, applied before any src module imports.

Every process this repo starts (server, CLI verbs, workers, collectors) reaches pydantic
only through a src import, so code here runs before pydantic's first import in all of them.
"""

import os

# The app registers no pydantic plugins and the venv ships none, so pydantic's per-process
# plugin discovery (importlib.metadata across every installed distribution) is pure import
# overhead; this is pydantic's documented opt-out, and setdefault keeps an operator's
# explicit setting authoritative.
os.environ.setdefault("PYDANTIC_DISABLE_PLUGINS", "__all__")
