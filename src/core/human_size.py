"""Human-readable byte counts, one home shared by the file listing and the trash reports."""

def format_size(size: int) -> str:
  """Render a byte count as ``"5 B"``, ``"2.0 KB"``, and upward with one decimal above bytes."""
  for unit in ("B", "KB", "MB", "GB", "TB"):
    if size < 1024:
      return f"{size} {unit}" if unit == "B" else f"{size:.1f} {unit}"
    size /= 1024
  return f"{size:.1f} PB"
