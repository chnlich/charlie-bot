The trigger's first line names the bound task.
When this session holds no specific reason to act otherwise, run that task's round with `charliebot delegate --bound-task <name>`. When it holds such a reason, act on that reason and state it in this session. Add a note only for a fact that the round needs; a note adds facts and leaves the round's repo, base branch and backend unchanged.
The server builds the worker from the task file and the bound session's backend. While a round of this task is in flight, the server answers 409 and starts nothing.
While a round of the task is in flight, or the host awaits login approval, the server skips the fire and this session receives no turn.
