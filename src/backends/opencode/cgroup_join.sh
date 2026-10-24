echo $$ > "$CHARLIEBOT_SESSION_CGROUP/cgroup.procs" || exit 97
echo "$CHARLIEBOT_SESSION_OOM_SCORE_ADJ" > "/proc/$$/oom_score_adj" || exit 97
unset BASH_ENV
