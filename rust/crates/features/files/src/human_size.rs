//! `src/infra/human_size.py`: the byte counts the directory listing prints.

/// `human_size.format_size`: `"5 B"`, `"2.0 KB"`, and upward with one decimal above bytes.
pub fn format_size(size: u64) -> String {
  if size < 1024 {
    return format!("{size} B");
  }
  // Python divides the int once, then divides the float by 1024 at each unit.
  let mut value = size as f64 / 1024.0;
  for unit in ["KB", "MB", "GB", "TB"] {
    if value < 1024.0 {
      return format!("{value:.1} {unit}");
    }
    value /= 1024.0;
  }
  format!("{value:.1} PB")
}

#[cfg(test)]
mod tests {
  use super::format_size;

  #[test]
  fn bytes_below_one_kilobyte_print_as_an_integer() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(1), "1 B");
    assert_eq!(format_size(1023), "1023 B");
  }

  #[test]
  fn each_unit_starts_at_its_power_of_1024() {
    assert_eq!(format_size(1024), "1.0 KB");
    assert_eq!(format_size(1024 * 1024), "1.0 MB");
    assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    assert_eq!(format_size(1024_u64.pow(4)), "1.0 TB");
    assert_eq!(format_size(1024_u64.pow(5)), "1.0 PB");
  }

  #[test]
  fn a_value_just_below_a_unit_boundary_rounds_up_into_the_smaller_unit() {
    // 1023.999 KB prints as "1024.0 KB", as Python's `:.1f` does.
    assert_eq!(format_size(1024 * 1024 - 1), "1024.0 KB");
  }

  #[test]
  fn one_decimal_rounds_half_to_even() {
    // 1.25 and 1.75 are exact in binary, so the tie goes to the even digit.
    assert_eq!(format_size(1280), "1.2 KB");
    assert_eq!(format_size(1792), "1.8 KB");
  }

  #[test]
  fn the_largest_count_stays_in_petabytes() {
    assert_eq!(format_size(u64::MAX), "16384.0 PB");
  }
}
