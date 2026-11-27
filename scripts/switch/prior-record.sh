# The prior switch's record, as the next switch's check A reads it: the server stop the record holds, and the server log
# that the stop led to. scripts/switch-server.sh sources this file, as it sources scripts/release/lock.sh. Each function
# prints what it finds and nothing else, so a caller that finds nothing gets an empty string.
#
#   prior_stop_line LOG          the record's last server stop as its timestamped line; nothing when it holds none
#   new_server_log DIR STAMP     the first server log in DIR that holds a launch at or after STAMP and is no later than
#                                now; nothing when no server launched in that span
#   server_launch_stamps LOG     the launch time of each server LOG holds, as YYYYMMDDHHMMSSmmm; status 1 and a message
#                                on stderr when LOG holds none

# prior_stop_line LOG: the record's last server stop, as its timestamped line. A stop is either the `stopping the old
# server` line of a stop that signalled the server, or the `no server listens` line of a stop step that found none.
# Prints nothing when the record holds neither, which means the switch stopped no server.
prior_stop_line() {
  local pattern='^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3} switch: '
  pattern+='(stopping the old server, |no server listens on port )'
  grep -E "$pattern" "$1" | tail -n 1
}

# server_launch_stamps LOG: the launch time of each server that LOG holds, to the millisecond, as YYYYMMDDHHMMSSmmm, in
# file order. The launcher writes a `launcher: implementation=` line at each launch (scripts/start-server.sh). A launch
# in the same second as an earlier one appends to that second's log, so one log can hold several launches. A log with no
# launch line is an error, not a log to skip: the function names LOG on stderr and returns 1.
server_launch_stamps() {
  local lines line stamp
  lines="$(grep -E '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3} launcher: implementation=' "$1")"
  if [ -z "$lines" ]; then
    echo "check A: server log $1 holds no launch line" >&2
    return 1
  fi
  while IFS= read -r line; do
    stamp="${line:0:23}"
    echo "${stamp//[^0-9]/}"
  done <<< "$lines"
}

# new_server_log DIR STAMP: the first server log in DIR that holds a launch at or after STAMP and is no later than now.
# STAMP is the local clock to the millisecond, as YYYYMMDDHHMMSSmmm. A log's name carries its launch's second, a coarse
# filter on the candidates; the launch lines in the log, to the millisecond, decide.
new_server_log() {
  local dir=$1 start=$2 end server second launches launch
  end="$(date '+%Y%m%d%H%M%S')"
  while IFS= read -r server; do
    second="${server##*/server_}"
    second="${second%.log}"
    second="${second//_/}"
    if [ "$second" -ge "${start:0:14}" ] && [ "$second" -le "$end" ]; then
      launches="$(server_launch_stamps "$server")" || return 1
      while IFS= read -r launch; do
        if [ "$launch" -ge "$start" ]; then
          echo "$server"
          return 0
        fi
      done <<< "$launches"
    fi
  done < <(find "$dir" -maxdepth 1 -name 'server_*.log' | sort)
  return 0
}
