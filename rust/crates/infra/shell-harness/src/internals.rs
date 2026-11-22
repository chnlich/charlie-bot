//! Private helpers and the stub scripts behind the public surface. Nothing here is re-exported from lib.rs.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

pub(crate) const SYSTEMCTL: &str = r#"case "$*" in
  *charliebot-agents.slice*)
    echo "ActiveState=${SYSTEMCTL_ACTIVE_STATE:-active}"
    echo "MemoryMax=${SYSTEMCTL_MEMORY_MAX:-{memory_max}}"
    echo "MemorySwapMax=${SYSTEMCTL_MEMORY_SWAP_MAX:-{memory_swap_max}}"
    echo "CPUWeight=${SYSTEMCTL_CPU_WEIGHT:-{cpu_weight}}"
    echo "IOWeight=${SYSTEMCTL_IO_WEIGHT:-{io_weight}}"
    ;;
  *)
    exit 1
    ;;
esac
"#;

pub(crate) const SYSTEMD_RUN: &str = r#"if [ "${SYSTEMD_RUN_REFUSE_ALL:-0}" = "1" ]; then
  echo "stub systemd-run: start refused by test" >&2
  exit 1
fi
arguments=("$@")
separator=-1
for index in "${!arguments[@]}"; do
  if [ "${arguments[index]}" = "--" ]; then
    separator=$index
    break
  fi
done
unit_name=""
for argument in "$@"; do
  case "$argument" in
    --unit=*) unit_name=${argument#--unit=} ;;
  esac
done
records="${BASH_SOURCE[0]%/*}/systemd-run.records"
if [ -f "$records" ] && cut -f1 "$records" | grep -Fxq -- "$unit_name"; then
  echo "Failed to start transient scope unit: Unit $unit_name.scope was already loaded or has a fragment file." >&2
  exit 1
fi
printf '%s' "$unit_name" >> "$records"
for argument in "$@"; do
  printf '\t%s' "$argument" >> "$records"
done
printf '\n' >> "$records"
exec "${arguments[@]:separator+1}"
"#;

pub(crate) fn read_text(reader: &mut impl Read) -> String {
  let mut bytes = Vec::new();
  reader.read_to_end(&mut bytes).expect("read the command's output");
  String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn exit_code(status: ExitStatus) -> i32 {
  // A command that a signal ended reports 128 plus the signal number, as a shell does.
  status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// One setting's value from a unit file: the last assignment wins, as it does in systemd.
pub(crate) fn unit_setting<'a>(text: &'a str, key: &str) -> Option<&'a str> {
  let prefix = format!("{key}=");
  text.lines().filter_map(|line| line.strip_prefix(prefix.as_str())).next_back()
}
