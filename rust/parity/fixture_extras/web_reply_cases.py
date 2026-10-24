"""Create the web reply parity sessions: a web manager with three child sessions, and the user messages of its For-you layer.

The three children wait on the user in three ways: one is open, one is archived with its status set, and one has its task
closed. Each status and each close goes through the server's writers, and each status carries a fixed stamp, so both homes
store the same status file.
"""

import datetime

from rust.parity.fixture import Builder
from src.features.web_reply import metadata
from src.infra import event_types as ET

# A fixed stamp for every status the extra writes, so both homes store the same file.
STATUS_STAMP = datetime.datetime(2026, 10, 16, 21, 5, 0, tzinfo=datetime.UTC)


def _user_message(text: str) -> dict:
  return {
      "type": ET.USER,
      "message": {
          "role": "user",
          "content": [{
              "type": "text",
              "text": text
          }]
      },
  }


async def _waiting(b: Builder, key: str, parent: str | None, name: str, text: str) -> str:
  """A child session whose status holds one Waiting on you item, written through the tree's slot writer."""
  session = await b.node(key, name, parent=parent)
  status = metadata.WebStatus(
      now="Waiting for the user",
      waiting=[metadata.WaitingItem(text=text, link="/panel")],
      updated_at=STATUS_STAMP,
  )
  await b.tree.update_slot_fields(session, "web_reply", web_status=status)
  return session


async def build(b: Builder) -> dict:
  root = await b.node("web_reply", "Web reply parity")
  await b.say(root, _user_message("Which layout should the panel use?"))
  await b.say(root, _user_message("Also check the gauge archives."))
  await _waiting(b, "web_open", "web_reply", "Open decisions", "Pick the panel layout")
  archived = await _waiting(b, "web_archived", "web_reply", "Archived decisions", "Confirm the archive policy")
  await b.lifecycle.archive_session(archived)
  # A parentless closed task: an archive reports to the parent, and a pending report would make startup recovery launch
  # the root, which the fixture's fixed-point check refuses. No web-status read lists an archived task.
  closed = await _waiting(b, "web_closed", None, "Closed decisions", "Approve the closed plan")
  await b.tree.completion.archive_subtree(
      closed,
      request_id="fixture-archive-web-closed",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=b.operator)
  return {"root": root}
