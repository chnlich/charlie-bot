# The prior switch's record, as the next switch's check A reads it: the server stop the record holds, and the server log
# that the stop led to. scripts/switch-server.sh sources this file, as it sources scripts/release/lock.sh. Each function
# prints what it finds and nothing else, so a caller that finds nothing gets an empty string.
#
#   prior_stop_line LOG          the record's last server stop as its timestamped line; nothing when it holds none
#   new_server_log DIR STAMP     the first server log in DIR launched at or after STAMP and no later than now; nothing
#                                when no server launched in that span

# prior_stop_line LOG: the record's last server stop, as its timestamped line. A stop is either the `stopping the old
# server` line of a stop that signalled the server, or the `no server listens` line of a stop step that found none.
# Prints nothing when the record holds neither, which means the switch stopped no server.
prior_stop_line() {
  local pattern='^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3} switch: '
  pattern+='(stopping the old server, |no server listens on port )'
  grep -E "$pattern" "$1" | tail -n 1
}

# new_server_log DIR STAMP: the first server log in DIR launched at or after STAMP and no later than now. STAMP and the
# log names carry the same local clock, as YYYYMMDDHHMMSS.
new_server_log() {
  local dir=$1 start=$2 end server stamp
  end="$(date '+%Y%m%d%H%M%S')"
  while IFS= read -r server; do
    stamp="${server##*/server_}"
    stamp="${stamp%.log}"
    stamp="${stamp//_/}"
    if [ "$stamp" -ge "$start" ] && [ "$stamp" -le "$end" ]; then
      echo "$server"
      return 0
    fi
  done < <(find "$dir" -maxdepth 1 -name 'server_*.log' | sort)
  return 0
}
