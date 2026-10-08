"""Cron's startup check: every task and step `backend` names a `backends.options` id."""

from src.infra.config import CharlieBotConfig, get_scheduled_tasks


def check_backend_refs(cfg: CharlieBotConfig) -> None:
  """Raise ValueError listing every cron task `backend` and step `backend` that names no option id.

  The server runs this at start through the wiring registry, after `require_backends`.
  A task or step without a backend stays valid. Each line names the task's cron.d file,
  the entry and the id.
  """
  ids = {option.id for option in cfg.backends.options}
  problems: list[str] = []
  for task in get_scheduled_tasks():
    task_file = cfg.config_d_dir / "cron.d" / f"{task.name}.yaml"
    refs = [("backend", task.backend)] + [(f"steps '{step.name}' backend", step.backend) for step in task.steps or []]
    for entry, backend_id in refs:
      if backend_id and backend_id not in ids:
        problems.append(f"{task_file}: {entry} names unknown backend '{backend_id}'")
  if problems:
    raise ValueError(
        "backend references must name a backends.options id (id rule: BackendsConfig in "
        "src/infra/config.py):\n" + "\n".join(f"  {problem}" for problem in problems))
