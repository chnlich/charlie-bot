"""Usage's startup check: every ``backends.options`` id attributes to its type's usage source.

The usage page attributes a charlie-bot row's account ids to the CLI that ran the call
(src/features/usage/api.py ``_account_source``): a registered id through its config type, an id
that left the config through the id prefix its source registers. An id outside every registered
prefix page-500s the moment it leaves the config, so the server refuses such an option at start
instead.
"""

from src.infra import config
from src.runtime.hooks import usage_source_registration


def check_backend_ids(cfg: config.CharlieBotConfig) -> None:
  """Raise ValueError listing every ``backends.options`` entry whose id its type's usage source
  cannot attribute.

  The server runs this at start through the wiring registry, after ``require_backends``. A type
  with no usage source is a violation, and so is an id starting with none of its source's
  registered prefixes. One error lists every violation; each line names the config file, the
  options index, the id, the type and the allowed prefixes.
  """
  problems: list[str] = []
  for index, option in enumerate(cfg.backends.options):
    source = usage_source_registration.source_for(str(option.type))
    if source is None:
      problems.append(
          f"{cfg.config_file}: backends.options[{index}] id '{option.id}' type '{option.type}' "
          "attributes to no usage source")
    elif not option.id.startswith(source.id_prefixes):
      problems.append(
          f"{cfg.config_file}: backends.options[{index}] id '{option.id}' type '{option.type}' starts "
          f"with none of the id prefixes {source.name} registers {source.id_prefixes}")
  if problems:
    raise ValueError(
        "backend option ids must attribute to their type's usage source (id rule: BackendsConfig in "
        "src/infra/config.py):\n" + "\n".join(f"  {problem}" for problem in problems))
