use std::process;

pub(super) fn exit_error(message: &str) -> ! {
  error_line(message);
  process::exit(1)
}

pub(super) fn error_line(message: &str) {
  eprintln!("{message}");
}
