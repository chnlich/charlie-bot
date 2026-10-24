"""Create synthetic sessions for session metadata write route parity."""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict:
  from src.infra import config, models
  from src.features.improve import improve_sequence
  from src.runtime import run_token
  access_key = config.configured_access_key()
  if not access_key:
    raise RuntimeError("fixture requires a configured synthetic access key")

  def queued_run(session_id: str, run_id: str, *, sequence_ref: models.SequenceRef | None = None) -> models.RunRecord:
    return models.RunRecord(
        id=run_id,
        session_id=session_id,
        kind="work",
        backend=b.option.id,
        model=b.option.model,
        sequence_ref=sequence_ref,
    )

  await b.node("write_group_a", "Write group A", group="Write original")
  await b.node("write_group_b", "Write group B", group="Write original")
  write_session = await b.node("write_session", "Write session", group="Write session group")
  archive_session = await b.node("write_archive", "Write archive")
  queued_archive = await b.node("write_queued_archive", "Write queued archive")
  # An unclaimed sequence run is never auto-dispatched but still blocks archive as queued work.
  queued_archive_run = queued_run(
      queued_archive,
      "write-queued-run",
      sequence_ref=models.SequenceRef(
          kind="improve", owner_ref=improve_sequence.loop_owner_ref(queued_archive, 1, b.cfg), position=0),
  )
  async with b.tree.control_lock:
    await b.tree.runs.register_run_locked(queued_archive_run, task_spec_text=None)

  permanent_session = await b.node("write_permanent", "Write permanent")
  permanent_content = await b.node("write_permanent_content", "Write permanent content")
  await b.say(
      permanent_content,
      {
          "type": "assistant",
          "id": "write-preserved-assistant",
          "content": "Synthetic preserved content."
      },
  )
  permanent_child_parent = await b.node("write_permanent_child_parent", "Write permanent child parent")
  await b.node("write_permanent_child", "Write permanent child", parent="write_permanent_child_parent")
  permanent_run = await b.node("write_permanent_run", "Write permanent run")
  permanent_run_record = queued_run(permanent_run, "write-permanent-run")
  async with b.tree.control_lock:
    await b.tree.runs.register_run_locked(permanent_run_record, task_spec_text=None)
  await b.tree.runs.request_stop(permanent_run, permanent_run_record.id, "write-permanent-run-stop")
  permanent_trigger = await b.node("write_permanent_trigger", "Write permanent trigger")
  trigger_dir = b.store.session_dir(permanent_trigger) / "triggers"
  trigger_dir.mkdir()
  (trigger_dir / "write-trigger.json").write_text("{}\n", encoding="utf-8")
  permanent_reference = await b.node("write_permanent_reference", "Write permanent reference")
  reference_session = await b.node("write_reference_session", "Write reference session")
  async with b.store.lock_for(reference_session):
    reference_meta = await b.store.get_session_bypassing_cache(reference_session)
    reference_meta.parent_session_id = permanent_reference
    await b.store.save_metadata(reference_meta, lock_held=True)
  permanent_report = await b.node("write_permanent_report", "Write permanent report")
  report_session = await b.node("write_report_session", "Write report session")
  await b.say(
      report_session,
      {
          "type": "child_report",
          "id": "write-child-report",
          "child_session_id": permanent_report,
          "child_event_id": "write-child-event",
          "content": "Synthetic child report.",
      },
  )
  await b.tree.completion.archive_subtree(
      report_session, request_id="fixture-archive-report-session", summary="archived by the user", result_refs=[],
      run_ids=[], caller=b.operator)

  agent_session = "write-agent-session"
  agent_run_id = "write-agent-run"
  agent_run = models.RunRecord(
      id=agent_run_id,
      session_id=agent_session,
      kind="work",
      backend=b.option.id,
      model=b.option.model,
      pid=4_194_401,
      pid_start="1",
  )
  async with b.tree.control_lock:
    await b.tree.runs.register_run_locked(agent_run, task_spec_text=None)
  token = run_token.sign_run_token(
      run_token.RunTokenClaims(session_id=agent_session, run_id=agent_run_id, agent="synthetic-agent"),
      access_key,
  )
  return {
      "write_session": write_session,
      "archive_session": archive_session,
      "queued_archive": queued_archive,
      "permanent_session": permanent_session,
      "permanent_content": permanent_content,
      "permanent_child_parent": permanent_child_parent,
      "permanent_run": permanent_run,
      "permanent_trigger": permanent_trigger,
      "permanent_reference": permanent_reference,
      "permanent_report": permanent_report,
      "agent_session": agent_session,
      "agent_token": token,
  }
