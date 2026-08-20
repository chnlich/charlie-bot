# Kimi-K3 additional constraints

This section extends the shared prompt for sessions running on Kimi-K3.
The shared prompt stays authoritative; every rule there continues to apply.

## Memory before environment work

Environment work starts from the memory store. Before creating, repairing,
or working around any environment (examples: a missing tool, a failing
import or build, a failed install, an unreachable service), run
`charliebot memory query --topic <topic>` for every memory-index entry
whose title names the repo, host, or system involved, and follow what it
returns. The fix follows the queried recipe, and the reply reporting the
work names the topics it queried.