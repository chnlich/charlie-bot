The trigger's first line names the bound task.
When this session holds no specific reason to act otherwise, run that task with `charliebot delegate --bound-task <name>`. When it holds such a reason, act on that reason and state it in this session. The task has no repo, so send no note: the server answers 400 to a note on it.
The server builds the worker from the task file. While a round of this task is in flight, the server answers 409 and starts nothing.
While a round of the task is in flight, or the host awaits login approval, the server skips the fire and this session receives no turn.
