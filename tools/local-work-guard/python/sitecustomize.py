"""The sitecustomize that the local work guard puts first on PYTHONPATH: refuses `-m unittest` on this host.

Every interpreter imports this module at start, so it checks the command line and does nothing else for a refused run.
Otherwise it loads the sitecustomize that this directory shadows, the system one when there is one, so an interpreter
starts as it did before the guard.
"""

import importlib.machinery
import importlib.util
import os
import sys

GUARD_PYTHON_DIR = os.path.dirname(os.path.abspath(__file__))
REFUSE = os.path.join(os.path.dirname(GUARD_PYTHON_DIR), "refuse.sh")


def runs_unittest(argv):
  return any(flag == "-m" and module == "unittest" for flag, module in zip(argv, argv[1:]))


if runs_unittest(sys.orig_argv):
  os.execvp("bash", ["bash", REFUSE, "python -m unittest"])

shadowed = [entry for entry in sys.path if os.path.abspath(entry) != GUARD_PYTHON_DIR]
spec = importlib.machinery.PathFinder.find_spec("sitecustomize", shadowed)
if spec is not None:
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
