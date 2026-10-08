"""The recap package's sidebar contribution: a session's recap memo goes when its runtime state does."""

from src.runtime.hooks.sidebar_contributions import SidebarContribution


class RecapSidebar(SidebarContribution):

  def drop_runtime_state(self, session_id: str) -> None:
    from src.features.recap import recap  # lazy: recap's import chain reaches src.runtime.sessions

    recap.drop_extract_memo(session_id)


contribution = RecapSidebar()
