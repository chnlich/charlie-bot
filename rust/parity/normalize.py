"""Normalization rules: every value two correct servers cannot produce identically, and nothing else.

Each field rule names the field it masks and the reason the value cannot match. A rule replaces the value with a
placeholder and never removes the field, so a field one server omits still shows as a difference. Five kinds
of rule live here:

- the tree path rule rewrites each server's temporary tree root inside every string, header value and file
  path, because each server owns its own tree (the harness gives both roots one length, so sizes, offsets
  and the ETags and revisions derived from them still compare exactly);
- the minted UUID rule leaves fixture UUIDs intact and maps each other UUID by first appearance in sorted paths
  and document order, because requests and runs create different random ids in each tree;
- the cron firing rule masks only the timestamp in a cron firing id or leaf name, and the scheduler's standalone
  `last_scheduled_run` field, because each server reads its own wall clock;
- field rules mask one JSON field of one route's response body or of one home file, or only the digits of a
  value of one form;
- text rules mask one pattern in one route's non-JSON body (a page that embeds JSON in its HTML);
- header rules name the response headers the comparison leaves out.

Add a rule only for a value that differs for a reason no implementation can control (wall-clock time, a random
id, a pid, a duration, a per-server path). A field that differs because of behavior is a parity difference.
"""

import dataclasses
import fnmatch
import json
import re
from typing import Any

import yaml

TREE_PLACEHOLDER = "<tree>"
_UUID = re.compile(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b")
_VOICE_STEM = re.compile(r"\b\d{4}-\d{2}-\d{2}T\d{6}\.\d{3}Z_[0-9a-f]{8}\b")
_ISO_WALL_CLOCK = r"(?P<date>\d{4}-\d{2}-\d{2}T)(?P<value>\d{2}:\d{2}:\d{2}(?:\.\d{6})?)(?P<offset>[+-]\d{2}:\d{2})"
# The ` · ` form is the cron leaf session name `<task name> · <firing>` that cron_sequence.py:257 writes. It reaches the
# compared files and responses as `SessionMetadata.name` (src/infra/models.py), in `run_token_claims.agent`
# (task_execution.py:1146, recorded in fake-agent/spawns.jsonl), and as the `- Session:` line in the rendered prompt
# (prompts/worker.md:3, task_prompts.render_session_info), which is also recorded in fake-agent stdin. Match the exact
# name in session and run-claim JSON, plus that fixed prompt line; loader.py _valid_cron_name defines its slug.
_CRON_FIRING_STAMP = re.compile(
    rf"(?P<cron_prefix>cron:[^:\s]+:){_ISO_WALL_CLOCK}"
    r"(?P<suffix>:(?:leaf|work|step:\d+))?(?=$|\s)")
_CRON_SESSION_LINE_STAMP = re.compile(
    rf"(?m)^- Session: (?P<leaf_name>[A-Za-z0-9][A-Za-z0-9._-]* · ){_ISO_WALL_CLOCK}(?=\r?$)")
_CRON_LEAF_NAME_STAMP = re.compile(rf"[A-Za-z0-9][A-Za-z0-9._-]* · {_ISO_WALL_CLOCK}")
_MERGED_TRACE_CACHE_PATH = re.compile(r"^home/cache/perfetto_merge/[0-9a-f]{64}\.json\.gz$")
_TEMP_DIR_SUFFIX = re.compile(r"\bcharliebot-(?:explain|oneshot|run)-(?P<value>[a-z0-9_]{8})(?![a-z0-9_])")
_TASK_BRANCH_TIMESTAMP = re.compile(r"\b(?P<prefix>charliebot(?:/|-)task-)(?P<value>\d{10})(?P<suffix>-[0-9a-f]{8})\b")
_OPENCODE_DEBUG_PORT = re.compile(r"(?P<prefix>http://127\.0\.0\.1:)(?P<value>\d+)(?P<suffix>/api/debug/location)")
_PIPE_INODE = re.compile(r"pipe:\[(?P<value>\d+)\]")


@dataclasses.dataclass(frozen=True)
class TextPatternRule:
  name: str
  pattern: re.Pattern[str]
  reason: str


VOICE_STEM_RULE = TextPatternRule(
    "voice-archive-stem",
    _VOICE_STEM,
    "wall-clock milliseconds and uuid4 hex: the voice archive names each recording at upload time",
)

# The prefix names the directory's role; tempfile.mkdtemp draws eight random characters from [a-z0-9_].
TEMP_DIR_SUFFIX_RULE = TextPatternRule(
    "temp-dir-suffix",
    _TEMP_DIR_SUFFIX,
    "tempfile.mkdtemp random suffix; the directory's role is fixed by its prefix",
)

TASK_BRANCH_TIMESTAMP_RULE = TextPatternRule(
    "task-branch-timestamp",
    _TASK_BRANCH_TIMESTAMP,
    "worktree branch names use time.time() seconds; independent servers can straddle a second",
)

OPENCODE_DEBUG_PORT_RULE = TextPatternRule(
    "opencode-debug-port",
    _OPENCODE_DEBUG_PORT,
    "the fake OpenCode API binds port 0, so each parity tree gets an OS-assigned loopback port",
)


@dataclasses.dataclass(frozen=True)
class PathPatternRule:
  name: str
  pattern: re.Pattern[str]
  reason: str

  @property
  def placeholder(self) -> str:
    return f"<{self.name}>"


MERGED_TRACE_CACHE_PATH_RULE = PathPatternRule(
    "merged-trace-cache-key",
    _MERGED_TRACE_CACHE_PATH,
    "the cache key hashes each tree's absolute trace paths and file times; the merged gzip bytes still compare",
)


@dataclasses.dataclass(frozen=True)
class FieldRule:
  """Mask *field* of the JSON documents *target* names.

  *target* is a route template ("GET /api/internal/version") for a response rule, or a glob over the
  tree-relative path ("home/state/writer_identity.json") of a JSON, JSONL or YAML file for a file rule.
  *field* is a dotted path; ``[*]`` steps into every list element and ``*`` into every mapping value. With
  *when*, the field is masked only in an object whose own fields hold those values: the object that names the
  field, so ``result_refs[*]`` is filtered by the event that holds ``result_refs``. With *pattern*, only the group
  ``value`` of a string that fully matches it is masked, and any other value compares as it is. With *values*, only
  a value of the same JSON type as one of them and equal to it is masked, null included. With *stop_request*, the
  field is masked only in a run record that this stop request interrupted (see ``Context.stop_requests``).
  """

  name: str
  target: str
  field: str
  reason: str
  when: tuple[tuple[str, str], ...]
  pattern: re.Pattern[str] | None = None
  sibling_prefix: tuple[tuple[str, str], ...] = ()
  values: tuple[Any, ...] | None = None
  stop_request: str | None = None

  @property
  def placeholder(self) -> str:
    return f"<{self.name}>"

  def mask(self, value: Any) -> Any:
    if self.values is not None:
      matched = any(type(value) is type(allowed) and value == allowed for allowed in self.values)
      return self.placeholder if matched else value
    if value is None:
      return None
    if self.pattern is None:
      return self.placeholder
    if not isinstance(value, str):
      raise TypeError(f"field rule {self.name!r} expects a string or null, got {type(value).__name__}")
    match = self.pattern.fullmatch(value)
    if match is None:
      return value
    return value[:match.start("value")] + self.placeholder + value[match.end("value"):]


@dataclasses.dataclass(frozen=True)
class TextRule:
  """Mask group ``value`` of every match of *pattern* in the non-JSON body of route *target*."""

  name: str
  target: str
  pattern: str
  reason: str

  @property
  def placeholder(self) -> str:
    return f"<{self.name}>"


_DELIVERY_STAMP = "the worker transcript stamps its closing run_delivery row with the time of the read"
_EXPLAIN_STAMP = (
    "wall-clock time: each server stamps an explanation's request and its finish on its own clock; "
    "a finish time is null until the generation ends")
# A finish time is null while its entry is pending, and that null stays compared. A `when` holds one value per key,
# so each ended state gets its own rule.
_ENDED_STATES = ("ready", "error")
_WEB_STATUS_STAMP = "wall-clock time of the write"
_SESSION_WRITE_STAMP = "wall-clock time of the write: each server stamps the session with its own clock"
_SESSION_WRITE_ROUTES = (
    "POST /api/sessions/",
    "POST /api/sessions/{session_id}/backend",
    "POST /api/sessions/{session_id}/fork",
    "POST /api/sessions/{session_id}/elone",
    "PATCH /api/sessions/{session_id}",
)

RESPONSE_RULES: tuple[FieldRule, ...] = (
    FieldRule(
        name="server-start-time",
        target="GET /api/internal/version",
        field="started_at",
        when=(),
        reason="wall-clock time: the moment each server process started",
    ),
    FieldRule(
        name="trigger-fire-time",
        target="POST /api/internal/schedule-trigger",
        field="fire_at",
        when=(),
        reason="wall-clock time: the trigger's due time is computed from each request's clock",
    ),
    *(
        FieldRule(
            name="session-write-updated-time",
            target=route,
            field="updated_at",
            when=(),
            reason="wall-clock time: each server stamps successful session metadata writes on its own clock",
        ) for route in (
            "POST /api/sessions/{session_id}/star",
            "POST /api/sessions/{session_id}/unstar",
            "POST /api/sessions/{session_id}/group",
            "POST /api/sessions/{session_id}/rounds/{round_id}/rate",
            "POST /api/sessions/{session_write_meta.write_session}/star",
            "POST /api/sessions/{session_write_meta.write_session}/unstar",
            "POST /api/sessions/{session_write_meta.write_session}/group",
            "POST /api/sessions/{session_write_meta.write_session}/rounds/write-round/rate",
        )),
    *(
        FieldRule(
            name="delivery-read-time",
            target=route,
            field="messages[*].timestamp",
            when=(("role", "run_delivery"),),
            reason=f"wall-clock time: {_DELIVERY_STAMP}",
        ) for route in (
            "GET /api/sessions/{session_id}/bootstrap",
            "GET /api/sessions/{session_id}/events",
            "GET /api/sessions/{session_id}/transcript",
        )),
    *(
        FieldRule(
            name=f"explain-{field.replace('_', '-')}",
            target=route,
            field=field,
            when=(),
            reason=_EXPLAIN_STAMP,
        ) for route in (
            "POST /api/sessions/{session_id}/explain",
            "GET /api/sessions/{session_id}/explain",
        ) for field in ("requested_at",)),
    *(
        FieldRule(
            name="explain-generated-at",
            target=route,
            field="generated_at",
            when=(("state", state),),
            reason=_EXPLAIN_STAMP,
        ) for route in (
            "POST /api/sessions/{session_id}/explain",
            "GET /api/sessions/{session_id}/explain",
        ) for state in _ENDED_STATES),
    *(
        FieldRule(
            name="explain-status-generated-time",
            target="GET /api/sessions/{session_id}/explain/status",
            field="*.generated_at",
            when=(("state", state),),
            reason=_EXPLAIN_STAMP,
        ) for state in _ENDED_STATES),
    FieldRule(
        name="web-status-set-time",
        target="POST /api/internal/web/status",
        field="updated_at",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-set-since",
        target="POST /api/internal/web/status",
        field="waiting[*].since",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-read-time",
        target="GET /api/internal/web/status",
        field="web_status.updated_at",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-read-since",
        target="GET /api/internal/web/status",
        field="web_status.waiting[*].since",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-read-descendant-time",
        target="GET /api/internal/web/status",
        field="descendants[*].updated_at",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    *(
        FieldRule(
            name=f"session-write-{field.replace('_', '-')}",
            target=target,
            field=field,
            when=(),
            reason=_SESSION_WRITE_STAMP,
        ) for target in _SESSION_WRITE_ROUTES for field in ("created_at", "updated_at")),
    FieldRule(
        name="session-runs-pid",
        target="GET /api/sessions/{session_id}/runs",
        field="items[*].pid",
        when=(),
        reason="pid: each server launches its own agent process",
    ),
    FieldRule(
        name="session-runs-pid-start",
        target="GET /api/sessions/{session_id}/runs",
        field="items[*].pid_start",
        when=(),
        reason="pid: the agent's /proc start time in clock ticks differs between processes",
    ),
    FieldRule(
        name="session-runs-started-at",
        target="GET /api/sessions/{session_id}/runs",
        field="items[*].started_at",
        when=(),
        reason="wall-clock time: each server records its agent launch on its own clock",
    ),
    FieldRule(
        name="session-runs-ended-at",
        target="GET /api/sessions/{session_id}/runs",
        field="items[*].ended_at",
        when=(),
        reason="wall-clock time: each server records its agent completion on its own clock",
    ),
    FieldRule(
        name="session-runs-local-repo-task-spec-hash",
        target="GET /api/sessions/{session_id}/runs",
        field="items[*].task_spec_hash",
        when=(("repo_path", "<tree>/workspace/worker-repo"),),
        reason="the canonical task spec includes the repository path inside each independent parity tree",
    ),
    FieldRule(
        name="plan-version-response-created-time",
        target="GET /api/sessions/{session_id}/plans",
        field="plans[*].versions[*].created_at",
        when=(),
        reason="wall-clock time: each server stamps its own plan version on its own clock",
    ),
)

WS_FRAME_RULES: dict[str, tuple[FieldRule, ...]] = {}

RESPONSE_TEXT_RULES: tuple[TextRule, ...] = (
    # The page embeds the session bootstrap as sorted-key JSON: the delivery row's run_id, task_state and
    # timestamp follow its role.
    TextRule(
        name="delivery-read-time",
        target="GET /",
        pattern=r'"role": "run_delivery", "run_id": "[^"]*", "task_state": "[^"]*", "timestamp": (?P<value>"[^"]*")',
        reason=f"wall-clock time: {_DELIVERY_STAMP}",
    ),
    # The token usage page stamps the time it renders, and the time its ledger read took, each on the serving clock.
    TextRule(
        name="token-usage-render-stamp",
        target="GET /token-usage",
        pattern=r"generated &middot; (?P<value>[^<]*)</span>",
        reason="wall-clock time: the page stamps the time it renders from the serving process's clock",
    ),
    TextRule(
        name="token-usage-read-time",
        target="GET /token-usage",
        pattern=r"Read from the ledger in <b>(?P<value>[0-9.]+)&thinsp;s</b>",
        reason="wall-clock time: the page reports how long its ledger read took on the serving process's clock",
    ),
)

FILE_RULES: tuple[FieldRule, ...] = (
    FieldRule(
        name="server-port",
        target="home/config.yaml",
        field="server.port",
        when=(),
        reason="each server listens on its own port, written into its own config by the harness",
    ),
    FieldRule(
        name="improve-loop-created-time",
        target="home/sessions/*/loops/*/state.json",
        field="created_at",
        when=(),
        reason="wall-clock time: each server records the loop's reservation on its own clock",
    ),
    FieldRule(
        name="improve-loop-server-pid",
        target="home/sessions/*/loops/*/state.json",
        field="server_pid",
        when=(),
        reason="pid: the loop records the process that reserved it, and each tree runs its own server",
    ),
    FieldRule(
        name="improve-child-spec-hash",
        target="home/sessions/*/data/chat_events.jsonl",
        field="task_spec_hash",
        when=(("request_id", "improve:1"),),
        reason="the canonical task spec includes the repository path inside each independent parity tree",
    ),
    FieldRule(
        name="holder-pid",
        target="home/state/writer_identity.json",
        field="pid",
        when=(),
        reason="pid: the writer fence names the process that holds it",
    ),
    FieldRule(
        name="holder-pid-start",
        target="home/state/writer_identity.json",
        field="pid_start",
        when=(),
        reason="pid: the holder's /proc start time in clock ticks since boot",
    ),
    FieldRule(
        name="holder-start-time",
        target="home/state/writer_identity.json",
        field="started_at",
        when=(),
        reason="wall-clock time: the moment the holder acquired the fence",
    ),
    FieldRule(
        name="holder-argv",
        target="home/state/writer_identity.json",
        field="argv",
        when=(),
        reason="the holder's own command line: each implementation is a different executable",
    ),
    FieldRule(
        name="run-pid",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="pid",
        when=(),
        reason="pid: each server launches its own agent process",
    ),
    FieldRule(
        name="run-pid-start",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="pid_start",
        when=(),
        reason="pid: the agent's /proc start time in clock ticks differs between processes",
    ),
    FieldRule(
        name="run-start-time",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="started_at",
        when=(),
        reason="wall-clock time: each server records its agent launch on its own clock",
    ),
    FieldRule(
        name="run-end-time",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="ended_at",
        when=(),
        reason="wall-clock time: each server records its agent completion on its own clock",
    ),
    FieldRule(
        name="interrupted-run-exit-code",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="exit_code",
        when=(),
        values=(None, -9),
        stop_request="synthetic-stop-request",
        reason=(
            "race for the first write of a stopped run's terminal record: the stopper records the interrupted outcome "
            "with no exit code (runs.py:1161) and the launcher records the signal exit -9, and record_finish_locked "
            "keeps the first write (runs.py:1036-1047), so Python's exit code is null or -9 by timing; the stop "
            "request id is handover-stop-request's own, so no other scenario's run is masked"),
    ),
    FieldRule(
        name="cancelled-run-exit-code",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="exit_code",
        when=(),
        values=(None, -15, -9),
        stop_request="synthetic-manager-run-cancel",
        reason=(
            "the same race as interrupted-run-exit-code, on the run cancel route: the stopper records the interrupted "
            "outcome with no exit code, and the launcher records the signal exit of the SIGTERM it sends (-15), or -9 "
            "when it escalates; the request id is agent-orchestration-cancel's own, so no other scenario's run is "
            "masked"),
    ),
    FieldRule(
        name="run-local-repo-task-spec-hash",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="task_spec_hash",
        when=(("repo_path", "<tree>/workspace/worker-repo"),),
        reason="the canonical task spec includes the repository path inside each independent parity tree",
    ),
    FieldRule(
        name="run-event-time",
        target="home/sessions/*/data/runs/*/events.jsonl",
        field="timestamp",
        when=(),
        reason="wall-clock time: each worker event is stamped by its own server",
    ),
    FieldRule(
        name="chat-event-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="timestamp",
        when=(),
        reason="wall-clock time: each process stamps its own chat and run facts",
    ),
    FieldRule(
        name="local-repo-task-created-spec-hash",
        target="home/sessions/*/data/chat_events.jsonl",
        field="task_spec_hash",
        when=(("request_id", "synthetic-delegate-implement-failure"),),
        reason="the canonical task spec includes the repository path inside each independent parity tree",
    ),
    *(
        FieldRule(
            name=f"{event_type}-landed-local-repo-spec-ref",
            target="home/sessions/*/data/chat_events.jsonl",
            field="result_refs[*]",
            when=(("type", event_type),),
            pattern=re.compile(r"spec:(?P<value>[0-9a-f]{64})"),
            sibling_prefix=(("result_refs", "landed:"),),
            reason="the canonical task spec hash includes the repository path inside each independent parity tree",
        ) for event_type in ("task_closed", "child_report")),
    FieldRule(
        name="master-done-total-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="total_ms",
        when=(("type", "master_done"),),
        reason="duration: server scheduling and trace timing differ between independent runs",
    ),
    FieldRule(
        name="master-done-first-event-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="spawn_to_first_event_ms",
        when=(("type", "master_done"),),
        reason="duration: process startup and event polling differ between independent runs",
    ),
    FieldRule(
        name="master-done-first-assistant-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="first_assistant_ms",
        when=(("type", "master_done"),),
        reason="duration: process startup and event polling differ between independent runs",
    ),
    FieldRule(
        name="master-done-thinking-seconds",
        target="home/sessions/*/data/chat_events.jsonl",
        field="thinking_seconds",
        when=(("type", "master_done"),),
        reason="duration: whole seconds of the busy interval, timed on each server's clock, differ between runs",
    ),
    FieldRule(
        name="session-updated-time",
        target="home/sessions/*/metadata.json",
        field="updated_at",
        when=(),
        reason="wall-clock time: the session records each server's latest event write",
    ),
    FieldRule(
        name="session-created-time",
        target="home/sessions/*/metadata.json",
        field="created_at",
        when=(),
        reason="wall-clock time: the session records its creation on each server's clock",
    ),
    FieldRule(
        name="cron-last-scheduled-run",
        target="home/sessions/*/metadata.json",
        field="last_scheduled_run",
        when=(),
        pattern=re.compile(_ISO_WALL_CLOCK),
        reason="wall-clock time: the scheduler records each server's cron firing on its own clock",
    ),
    FieldRule(
        name="web-status-file-time",
        target="home/sessions/*/metadata.json",
        field="web_status.updated_at",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-file-since",
        target="home/sessions/*/metadata.json",
        field="web_status.waiting[*].since",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-event-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="web_status.updated_at",
        # `when` sees the status object that holds this leaf; only web_status events carry that object.
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-event-since",
        target="home/sessions/*/data/chat_events.jsonl",
        field="web_status.waiting[*].since",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="session-claude-start-time",
        target="home/sessions/*/metadata.json",
        field="cc_session_started_at",
        when=(),
        reason="wall-clock time: the session records when each server created its Claude conversation",
    ),
    *(
        FieldRule(
            name=f"trigger-{field.replace('_', '-')}",
            target="home/sessions/*/triggers/*.json",
            field=field,
            when=(),
            reason=f"wall-clock time: the trigger store records {field} on each server's clock",
        ) for field in ("created_at", "fire_at", "fired_at")),
    FieldRule(
        name="plan-takeoff-time",
        target="home/sessions/*/plans.json",
        field="plans[*].takeoff.at",
        when=(),
        reason="wall-clock time: each server stamps its own plan approval on its own clock",
    ),
    FieldRule(
        name="plan-close-time",
        target="home/sessions/*/plans.json",
        field="plans[*].closed.at",
        when=(),
        reason="wall-clock time: each server stamps its own plan close on its own clock",
    ),
    FieldRule(
        name="plan-version-created-time",
        target="home/sessions/*/plans.json",
        field="plans[*].versions[*].created_at",
        when=(),
        reason="wall-clock time: each server stamps its own plan version on its own clock",
    ),
    FieldRule(
        name="explain-result-requested-time",
        target="home/sessions/*/data/explain_results.json",
        field="*.requested_at",
        when=(),
        reason=_EXPLAIN_STAMP,
    ),
    *(
        FieldRule(
            name="explain-result-generated-time",
            target="home/sessions/*/data/explain_results.json",
            field="*.generated_at",
            when=(("state", state),),
            reason=_EXPLAIN_STAMP,
        ) for state in _ENDED_STATES),
    FieldRule(
        name="fake-agent-pid",
        target="fake-agent/spawns.jsonl",
        field="pid",
        when=(),
        reason="pid: each harness tree starts its fake agent as a different process",
    ),
    FieldRule(
        name="fake-agent-pid-start",
        target="fake-agent/spawns.jsonl",
        field="pid_start",
        when=(),
        reason="pid: each fake agent has a different /proc start time in clock ticks",
    ),
    FieldRule(
        name="fake-agent-stdout-inode",
        target="fake-agent/spawns.jsonl",
        field="stdout_path",
        pattern=_PIPE_INODE,
        when=(),
        reason="pipe inode: the kernel numbers each tree's pipe apart; a file path still compares",
    ),
    FieldRule(
        name="fake-agent-pgid",
        target="fake-agent/spawns.jsonl",
        field="pgid",
        when=(),
        reason="pid: each fake agent leads its own process group, so its pgid is its pid in each harness tree",
    ),
)

# Response headers outside the comparison, each with the reason it cannot match or carries nothing a client
# reads. Every other header is compared after the tree path rule.
IGNORED_HEADERS: dict[str, str] = {
    "date": "wall-clock time of the response",
    "server": "the HTTP stack's own banner; no client reads it",
    "content-length":
        "framing: the decoded body is compared, and a compressed body's length depends on the "
        "compressor",
    "transfer-encoding": "framing: chunked or sized delivery of the same body",
    "connection": "framing: connection reuse",
    "keep-alive": "framing: connection reuse",
}


@dataclasses.dataclass(frozen=True)
class ResponseHeaderRule:
  name: str
  target: str
  header: str
  reason: str

  @property
  def placeholder(self) -> str:
    return f"<{self.name}>"


RESPONSE_HEADER_RULES: tuple[ResponseHeaderRule, ...] = (
    ResponseHeaderRule(
        "merged-trace-etag",
        "GET /perfetto/merged",
        "etag",
        "the generated gzip file's mtime and size; its tree-specific path has a separate cache file",
    ),
    ResponseHeaderRule(
        "merged-trace-last-modified",
        "GET /perfetto/merged",
        "last-modified",
        "wall-clock time when each server created its identical merged gzip bytes",
    ),
)

# The headers each fake's feature code sets explicitly on its REST requests. No feature client sets a header on a
# WebSocket connect, so WebSocket handshakes compare none.
EXPLICIT_REQUEST_HEADERS: dict[str, tuple[str, ...]] = {
    "slack": ("authorization",),
    "discord": ("authorization", "user-agent"),
    "gemini": (),
    "muse": (),
    "openai_compatible": ("authorization",),
}

# Request headers compared by media type, lowercased and without parameters.
MEDIA_TYPE_REQUEST_HEADERS: dict[str, str] = {
    "content-type":
        "the media type names the body encoding the feature code chose; parameters such as the charset "
        "and the multipart boundary are library defaults and are dropped",
}

IGNORED_REQUEST_HEADERS: dict[str, str] = {
    "host": "transport routing: CONNECT already chooses the fake host; it carries no request behavior",
    "accept": "HTTP client default: the feature request does not set this header",
    "user-agent": "library default on Slack REST calls and on every WebSocket handshake; only Discord REST sets it",
    "accept-encoding": "HTTP client default: automatic response decoding is outside the feature request",
    "content-length": "framing: the parsed request body is compared structurally",
    "connection": "framing: HTTP connection reuse is independent of the feature request",
    "keep-alive": "framing: HTTP connection reuse is independent of the feature request",
    "expect": "transport negotiation: no feature request depends on a 100-continue exchange",
    "proxy-connection": "proxy transport: generated for the local CONNECT tunnel",
    "upgrade": "WebSocket framing: handshake type is already recorded separately",
    "sec-websocket-key": "WebSocket handshake nonce: required by the protocol and random per connection",
    "sec-websocket-version": "WebSocket protocol version: fixed negotiation metadata",
    "sec-websocket-extensions": "WebSocket extension negotiation: library-generated framing metadata",
}


@dataclasses.dataclass(frozen=True)
class IgnoredMessageRule:
  name: str
  fake: str
  direction: str
  field: str
  value: Any
  reason: str


IGNORED_MESSAGE_RULES: tuple[IgnoredMessageRule, ...] = (
    IgnoredMessageRule(
        "discord-heartbeat",
        "discord",
        "server_to_fake",
        "op",
        1,
        "Discord op 1 heartbeat timing includes an unseeded random first interval",
    ),)


def ignored_fake_message(record: dict) -> bool:
  """Whether a recorded WebSocket message matches a narrowly named ignored-frame rule."""
  message = record.get("message")
  if not isinstance(message, dict):
    return False
  return any(
      record.get("fake") == rule.fake and record.get("direction") == rule.direction and
      message.get(rule.field) == rule.value for rule in IGNORED_MESSAGE_RULES)


@dataclasses.dataclass
class Context:
  """One server's per-run values: the names the tree path rule rewrites and the run facts a FieldRule reads.

  *interrupted_runs* holds the run ids whose run_finished outcome is interrupted, and *stop_requests* the
  (request_id, run_id) pairs of the durable run_stop_requested facts. Both hold the raw ids of the chat events.
  """

  tree_root: str
  fixture_uuids: frozenset[str] = frozenset()
  stable_uuids: dict[str, str] = dataclasses.field(default_factory=dict)
  minted_uuids: dict[str, str] = dataclasses.field(default_factory=dict)
  interrupted_runs: set[str] = dataclasses.field(default_factory=set)
  stop_requests: set[tuple[str, str]] = dataclasses.field(default_factory=set)


def fixture_uuids(files: list[tuple[str, bytes]]) -> frozenset[str]:
  """UUID strings already present in the fixture tree's paths and file contents."""
  found: set[str] = set()
  for path, data in files:
    found.update(_UUID.findall(path))
    found.update(_UUID.findall(data.decode("utf-8", errors="surrogateescape")))
  return frozenset(found)


def is_uuid(text: str) -> bool:
  """Report whether the whole text is one UUID, in the form the harness normalizes."""
  return _UUID.fullmatch(text) is not None


def _observe(value: Any, ctx: Context) -> None:
  if isinstance(value, str):
    for match in _UUID.finditer(value):
      identifier = match[0]
      if identifier not in ctx.fixture_uuids and identifier not in ctx.stable_uuids:
        ctx.minted_uuids.setdefault(identifier, f"<minted-{len(ctx.minted_uuids) + 1}>")
  elif isinstance(value, list):
    for item in value:
      _observe(item, ctx)
  elif isinstance(value, dict):
    for key, item in value.items():
      _observe(key, ctx)
      _observe(item, ctx)


def path_sort_key(path: str, ctx: Context) -> str:
  """The path with every UUID swapped for its stable name, so raw random bytes set no ordering.

  A minted UUID's number must not depend on where its raw form sorts: two trees' ids differ, and a
  clock-derived id sorts at a different place in each. Names the harness knows (fixture and stable
  aliases) keep their own text; every other UUID collapses to one token, so paths that differ only
  by their unknown UUIDs tie and their raw order breaks the tie.
  """

  def replace(match: re.Match[str]) -> str:
    identifier = match[0]
    if identifier in ctx.stable_uuids:
      return ctx.stable_uuids[identifier]
    if identifier in ctx.fixture_uuids:
      return identifier
    return "<uuid>"

  return _UUID.sub(replace, path)


def _observe_run_facts(value: Any, ctx: Context) -> None:
  """Record the stop requests and the interrupted finishes that a stop-request FieldRule reads."""
  if not isinstance(value, dict):
    return
  if value.get("type") == "run_stop_requested":
    ctx.stop_requests.add((value["request_id"], value["run_id"]))
  elif value.get("type") == "run_finished" and value.get("outcome") == "interrupted":
    ctx.interrupted_runs.add(value["run_id"])


def prepare_snapshot(entries: list[tuple[str, str, bytes]], ctx: Context) -> None:
  """Assign minted UUID names by stable-name path order and each JSON document's stored order."""
  ordered = sorted(entries, key=lambda entry: (path_sort_key(entry[0], ctx), entry[0]))
  for path, kind, data in ordered:
    _observe(path, ctx)
    if kind != "file":
      continue
    if path.endswith(".json"):
      _observe(json.loads(data), ctx)
    elif path.endswith(".jsonl"):
      for line in data.splitlines():
        if line.strip():
          try:
            value = json.loads(line)
          except json.JSONDecodeError:
            continue
          _observe(value, ctx)
          if path.endswith("/data/chat_events.jsonl"):
            _observe_run_facts(value, ctx)
    elif path.endswith((".yaml", ".yml")):
      _observe(yaml.safe_load(data), ctx)
    else:
      _observe(data.decode("utf-8", errors="surrogateescape"), ctx)


def _mask_temp_dir_suffix(match: re.Match[str]) -> str:
  """The match up to its suffix, then the placeholder: the prefix still names the directory's role."""
  return match[0][:match.start("value") - match.start()] + f"<{TEMP_DIR_SUFFIX_RULE.name}>"


def _mask_task_branch_timestamp(match: re.Match[str]) -> str:
  """The branch prefix and run suffix stay visible; only the independent servers' wall-clock second is replaced."""
  return f"{match['prefix']}<{TASK_BRANCH_TIMESTAMP_RULE.name}>{match['suffix']}"


def _mask_opencode_debug_port(match: re.Match[str]) -> str:
  """Keep the fake OpenCode endpoint visible while replacing only its OS-assigned port."""
  return f"{match['prefix']}<{OPENCODE_DEBUG_PORT_RULE.name}>{match['suffix']}"


def normalize_text(text: str, ctx: Context) -> str:
  """The tree path, minted UUID, scratch directory and task-branch timestamp read as placeholders."""
  normalized = text.replace(ctx.tree_root, TREE_PLACEHOLDER)

  def replace_uuid(match: re.Match[str]) -> str:
    identifier = match[0]
    if identifier in ctx.fixture_uuids:
      return identifier
    if identifier in ctx.stable_uuids:
      return ctx.stable_uuids[identifier]
    return ctx.minted_uuids.setdefault(identifier, f"<minted-{len(ctx.minted_uuids) + 1}>")

  normalized = VOICE_STEM_RULE.pattern.sub(f"<{VOICE_STEM_RULE.name}>", normalized)
  normalized = TEMP_DIR_SUFFIX_RULE.pattern.sub(_mask_temp_dir_suffix, normalized)
  normalized = TASK_BRANCH_TIMESTAMP_RULE.pattern.sub(_mask_task_branch_timestamp, normalized)
  normalized = OPENCODE_DEBUG_PORT_RULE.pattern.sub(_mask_opencode_debug_port, normalized)
  normalized = _CRON_FIRING_STAMP.sub(
      lambda match: f"{match['cron_prefix']}{match['date']}<cron-firing-time>{match['offset']}{match['suffix'] or ''}",
      normalized)
  normalized = _CRON_SESSION_LINE_STAMP.sub(
      lambda match: f"- Session: {match['leaf_name']}{match['date']}<cron-firing-time>{match['offset']}", normalized)
  return _UUID.sub(replace_uuid, normalized)


def normalize_path(path: str, ctx: Context) -> str:
  """Normalize one tree-relative or absolute file path, including the voice archive name rule."""
  normalized = normalize_text(path, ctx)
  if MERGED_TRACE_CACHE_PATH_RULE.pattern.fullmatch(normalized):
    return MERGED_TRACE_CACHE_PATH_RULE.placeholder
  return normalized


def _normalize_strings(value: Any, ctx: Context) -> Any:
  if isinstance(value, str):
    return normalize_text(value, ctx)
  if isinstance(value, list):
    return [_normalize_strings(item, ctx) for item in value]
  if isinstance(value, dict):
    is_session = {"id", "name", "profile", "task_parent_id"} <= value.keys()
    is_run_token_claims = {"agent", "run_id", "session_id"} <= value.keys()
    normalized = {}
    for key, item in value.items():
      normalized_key = normalize_text(key, ctx)
      normalized_item = _normalize_strings(item, ctx)
      is_cron_leaf_name = (is_session and key == "name") or (is_run_token_claims and key == "agent")
      if is_cron_leaf_name and isinstance(normalized_item, str):
        match = _CRON_LEAF_NAME_STAMP.fullmatch(normalized_item)
        if match is not None:
          normalized_item = (
              normalized_item[:match.start("value")] + "<cron-firing-time>" + normalized_item[match.end("value"):])
      normalized[normalized_key] = normalized_item
    return normalized
  return value


def _mask(value: Any, segments: list[str], rule: FieldRule) -> Any:
  if not segments:
    return rule.mask(value)
  head, rest = segments[0], segments[1:]
  if head == "[*]":
    return [_mask(item, rest, rule) for item in value] if isinstance(value, list) else value
  if not isinstance(value, dict):
    return value
  if head == "*":
    return {key: _mask(item, rest, rule) for key, item in value.items()}
  if head not in value:
    return value
  if all(segment == "[*]" for segment in rest) and any(value.get(key) != wanted for key, wanted in rule.when):
    return value
  return {**value, head: _mask(value[head], rest, rule)}


def _split_field(field: str) -> list[str]:
  segments: list[str] = []
  for part in field.split("."):
    if part.endswith("[*]"):
      if part[:-3]:
        segments.append(part[:-3])
      segments.append("[*]")
    else:
      segments.append(part)
  return segments


def _stopped_by(record: Any, request_id: str, ctx: Context) -> bool:
  """Whether *record* is a run that *request_id* stopped and that finished interrupted.

  The run id is read before the tree path rule rewrites it, because the context holds the raw ids of the chat events.
  """
  run_id = record.get("id") if isinstance(record, dict) else None
  return run_id in ctx.interrupted_runs and (request_id, run_id) in ctx.stop_requests


def normalize_json(value: Any, rules: tuple[FieldRule, ...], ctx: Context) -> Any:
  """Apply the tree path rule to every string, then matching field rules in *rules*."""
  record = value
  value = _normalize_strings(value, ctx)
  for rule in rules:
    if rule.sibling_prefix and (not isinstance(value, dict) or
                                any(not any(isinstance(item, str) and item.startswith(prefix)
                                            for item in value.get(field) or [])
                                    for field, prefix in rule.sibling_prefix)):
      continue
    if rule.stop_request is not None and not _stopped_by(record, rule.stop_request, ctx):
      continue
    value = _mask(value, _split_field(rule.field), rule)
  return value


def normalize_body_bytes(body: bytes, route: str, ctx: Context) -> bytes:
  """A non-JSON body: the tree path rule, then every text rule of *route*."""
  body = normalize_text(body.decode("utf-8", errors="surrogateescape"), ctx).encode("utf-8", errors="surrogateescape")
  for rule in RESPONSE_TEXT_RULES:
    if rule.target == route:
      placeholder = f'"{rule.placeholder}"'.encode()

      def mask(match: re.Match[bytes], placeholder: bytes = placeholder) -> bytes:
        start, end = match.span("value")
        return match.string[match.start():start] + placeholder + match.string[end:match.end()]

      body = re.sub(rule.pattern.encode(), mask, body)
  return body


# The message id the proxy mints for each answer: `msg_` and 32 lowercase hex digits, from uuid4. It is the `id` of a
# proxy response's JSON answer and of a stream's message_start frame, and each server mints its own, so the rule masks
# it there and nowhere else. An id the upstream supplies has another form and stays visible: a proxy that copied the
# upstream's id would still show as a difference.
PROXY_MESSAGE_ID = re.compile(rb'"id":"msg_[0-9a-f]{32}"')
PROXY_MESSAGE_ID_PLACEHOLDER = b'"id":"<proxy-message-id>"'
PROXY_ROUTE = re.compile(r"POST /api/anthropic-proxy/openai-compatible/[^/]+/v1/messages")


def mask_proxy_message_ids(body: bytes, route: str) -> bytes:
  """Mask each message id a proxy route minted in its body; the other routes' bodies are returned unchanged."""
  if PROXY_ROUTE.fullmatch(route) is None:
    return body
  return PROXY_MESSAGE_ID.sub(PROXY_MESSAGE_ID_PLACEHOLDER, body)


def response_rules(route: str) -> tuple[FieldRule, ...]:
  return tuple(rule for rule in RESPONSE_RULES if rule.target == route)


def ws_frame_rules(route: str) -> tuple[FieldRule, ...]:
  """Frame rules are keyed by the WebSocket route template, matching response-rule selection."""
  return WS_FRAME_RULES.get(route, ())


def file_rules(relative_path: str) -> tuple[FieldRule, ...]:
  return tuple(rule for rule in FILE_RULES if fnmatch.fnmatchcase(relative_path, rule.target))


def normalize_headers(headers: list[tuple[str, str]], ctx: Context, route: str) -> dict[str, list[str]]:
  """The compared headers: lowercased names, ignored ones dropped, values under the tree path rule."""
  compared: dict[str, list[str]] = {}
  for name, value in headers:
    lowered = name.lower()
    if lowered in IGNORED_HEADERS:
      continue
    rule = next(
        (rule for rule in RESPONSE_HEADER_RULES if rule.target == route and rule.header == lowered),
        None,
    )
    compared.setdefault(lowered, []).append(rule.placeholder if rule else normalize_text(value, ctx))
  return compared


def _media_type(value: str) -> str:
  """A Content-Type value without its parameters, lowercased."""
  return value.split(";", 1)[0].strip().lower()


def request_headers(
    fake: str,
    headers: dict[str, list[str]],
    ctx: Context,
    websocket: bool,
) -> dict[str, list[str]]:
  """The headers the feature code sets explicitly, and each media-type header's media type.

  Every other header must be named in IGNORED_REQUEST_HEADERS.
  """
  explicit = () if websocket else EXPLICIT_REQUEST_HEADERS[fake]
  compared: dict[str, list[str]] = {}
  for name, values in headers.items():
    if name in explicit:
      compared[name] = [normalize_text(value, ctx) for value in values]
    elif name in MEDIA_TYPE_REQUEST_HEADERS:
      compared[name] = [_media_type(value) for value in values]
    elif name not in IGNORED_REQUEST_HEADERS:
      raise ValueError(f"{fake} request header {name!r} is neither set explicitly nor named in IGNORED_REQUEST_HEADERS")
  return compared
