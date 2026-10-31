#!/usr/bin/env python3
"""Generate a synthetic session home, or read one back from a running server.

  mock-home.py generate <output dir> [--sessions N] [--seed S]
  mock-home.py check <output dir> <server base URL>

generate writes one directory per session, each with metadata.json and data/chat_events.jsonl, and writes
manifest.json beside them. The event-log sizes follow the production shape in SIZE_KNOTS. Every sentence is written
for this file: no text comes from a real session. One seed gives byte-identical output, so the servers under test
load the same data.

check reads the session count from GET /api/sessions/. That route reads only metadata.json, so check also reads the
newest message of the largest session and of two sessions that the seed picks, from GET /api/sessions/<id>/events.
It exits 1 on any mismatch with manifest.json. When CHARLIEBOT_SERVE_AUTH_HEADER is set, it sends that
`Name: value` header with each request.
"""

import argparse
import datetime
import json
import math
import os
import random
import sys
import urllib.error
import urllib.request
import uuid

PRODUCTION_SESSIONS = 7160
DEFAULT_SEED = 1
MANIFEST_NAME = 'manifest.json'
METADATA_NAME = 'metadata.json'
EVENT_LOG_PARTS = ('data', 'chat_events.jsonl')
# Every session starts inside this past window. A fixed anchor keeps the seed reproducible and keeps the pages from
# showing sessions dated after the run.
FIRST_SESSION_TIME = datetime.datetime(2025, 9, 1)
SESSION_START_SPREAD_SECONDS = 200 * 86400
HTTP_TIMEOUT_SECONDS = 120
# The `before` cursor that names the newest page: the server clamps it to the session's message count.
NEWEST_PAGE_BEFORE = 2**31 - 1
# Above this many event-log bytes left to fill, an assistant event carries long answers.
LONG_EVENT_BUDGET = 200000
# The synthetic blocks and tool-output lines that events draw from, composed once per run.
BLOCK_POOL_SIZE = 1500
OUTPUT_LINE_POOL_SIZE = 800
# A log's last gap smaller than this takes no more events; one filler message fills the gap.
FILLER_THRESHOLD_BYTES = 4096

# The event-log size at quantile u, log-linear between these knots. The knots are the production numbers: median
# 1.7 KB, p90 1.4 MB, p99 17 MB, and 144 MB for the largest of 7,160 sessions. The exponent of an upper segment sets
# how its sizes bunch toward the lower knot; the exponents give a total of about 5.8 GB at 7,160 sessions.
SIZE_KNOTS = ((0.0, 150.0), (0.5, 1.7e3), (0.9, 1.4e6), (0.99, 1.7e7), (1.0, 1.44e8))
SEGMENT_EXPONENTS = (1.0, 1.0, 2.0, 4.0)

ZH_SENTENCES = (
  '我们先把请求入口拆成解析、校验和分发三段。',
  '这个缓存的失效规则目前只看时间，还需要加上版本号。',
  '测试数据要覆盖空字段、超长字段和重复的键。',
  '列表在窄屏上会把按钮挤到第二行，先看看间距怎么定。',
  '调度器每次只取一个待办，处理完再取下一个。',
  '日志里有两次相同的超时，重试之间可能缺少退避。',
  '示例文档还在用旧的参数名，改完后要再跑一遍检查。',
  '这一步的输出只是中间结果，不需要写进最终报告。',
  '没有命中索引时退回全表扫描，并把耗时记入统计。',
  '先整理现有分支，再决定哪些改动可以合并。',
  '页面一次取回全部记录，数据量大时应改成分页。',
  '默认值只写在一个地方，别在三个文件里各写一份。',
  '这个函数有三种返回情况，调用方只处理了两种。',
  '压缩后的响应可以复用，不必每次请求都重新编码。',
  '失败的任务会留下标记文件，下次启动时清理它。',
  '状态转移用表格写出来，每行列出前置条件和动作。',
  '新增字段默认为空，旧数据读出来也不会报错。',
  '这里的锁粒度太粗，读请求也要等写请求结束。',
  '时间显示应以用户的时区为准，而不是服务器的时区。',
  '最后把结果写回文件，并确认文件权限是六百。',
  '这个模块只做转换，不应该直接访问磁盘。',
  '输入框的提示文字太长，换行后会遮住下面的按钮。',
  '回归测试放在同一个文件里，名字要说清楚场景。',
  '上一轮的数字偏高，先找出是哪一个步骤引入的。',
)
EN_SENTENCES = (
  'The handler parses the request body once and passes the result down.',
  'Retries back off exponentially, with a cap of thirty seconds.',
  'The fixture covers empty fields, very long names and repeated keys.',
  'Two readers share the cache, so the writer must publish a new generation.',
  'A table scan is cheap at this size, but the index will matter later.',
  'Keep the public signature unchanged and move the branch inside the function.',
  'A stale lock file blocks the next run until its owner process exits.',
  'We take the median of fifty requests after five warm-up calls.',
  'The renderer escapes angle brackets before it writes the table.',
  'Errors are logged once at the boundary instead of at every layer.',
  'Each worker writes its own file, so no two writers share a path.',
  'The default timeout lives in one place and every caller reads it.',
  'Unit tests cover the boundary values; the integration test covers the flow.',
  'The summary lists the changed files, the commands run and the results.',
  'This change only moves code, so the behavior of the function stays the same.',
  'Please confirm the expected order before the two branches are merged.',
  'The memory peak stays flat once the cache reaches its size limit.',
  'Thread safety depends on the single writer guard taken at startup.',
  'The sidebar shows only active sessions and loads archived rows on demand.',
  'Remove the helper after the last caller moves to the new module.',
  'The chart reads the same counters that the status page reads.',
  'Pagination keeps each response small when a list has many rows.',
  'The retry budget resets when the run reaches a terminal state.',
  'A reviewer can rerun the command in the table and compare the output.',
)
ZH_TOPICS = (
  '缓存层整理', '调度器超时', '会话列表分页', '发布检查清单', '日志格式统一', '配置项迁移',
  '上传文件校验', '侧边栏布局', '备份与恢复', '指标面板', '权限边界梳理', '数据表迁移',
)
EN_TOPICS = (
  'cache layer cleanup', 'scheduler timeout audit', 'session list paging', 'release checklist',
  'log format unification', 'config key migration', 'upload validation', 'sidebar layout pass',
  'backup and restore', 'metrics panel', 'permission boundary review', 'schema migration',
)
CODE_SNIPPETS = (
  ('python', 'def merge_rows(rows):\n    seen = set()\n    return [r for r in rows if not (r["id"] in seen or seen.add(r["id"]))]'),
  ('rust', 'fn retry_delay(attempt: u32) -> Duration {\n    Duration::from_millis(250u64 << attempt.min(7))\n}'),
  ('typescript', 'export function pageOf<T>(items: T[], size: number): T[][] {\n  const pages: T[][] = [];\n  for (let i = 0; i < items.length; i += size) pages.push(items.slice(i, i + size));\n  return pages;\n}'),
  ('bash', 'cargo test --workspace --locked\ncargo clippy --workspace --all-targets -- -D warnings'),
  ('sql', "SELECT id, name FROM sessions WHERE status = 'active' ORDER BY updated_at DESC LIMIT 40;"),
)
TOOL_NAMES = ('Read', 'Bash', 'Grep', 'Edit', 'Glob')


def positive_int(text):
  value = int(text)
  if value < 1:
    raise argparse.ArgumentTypeError(f'must be at least 1: {text}')
  return value


def event_log_size(u):
  """Return the production event-log size in bytes at quantile u, for 0 <= u <= 1."""
  for segment, exponent in enumerate(SEGMENT_EXPONENTS):
    (u_low, size_low), (u_high, size_high) = SIZE_KNOTS[segment], SIZE_KNOTS[segment + 1]
    if u <= u_high:
      position = (u - u_low) / (u_high - u_low)
      log_size = math.log(size_low) + (math.log(size_high) - math.log(size_low)) * position ** exponent
      return math.exp(log_size)
  raise ValueError(f'quantile {u} is above 1')


def planned_sizes(count, rng):
  """Return count sizes at evenly spaced quantiles, shuffled so that the seed picks each session's size."""
  sizes = []
  for index in range(count):
    u = index / (count - 1) if count > 1 else 0.5
    sizes.append(int(round(event_log_size(u))))
  rng.shuffle(sizes)
  return sizes


def stamp(moment):
  return moment.strftime('%Y-%m-%dT%H:%M:%S.%fZ')


def event_stamp(started_at, index):
  return (started_at + datetime.timedelta(seconds=index)).strftime('%Y-%m-%dT%H:%M:%SZ')


class SyntheticText:
  """Synthetic prose, markdown, code and tool traffic, composed from the sentences above.

  The run composes BLOCK_POOL_SIZE blocks and OUTPUT_LINE_POOL_SIZE tool-output lines once. Events draw from these
  pools, so the cost of an event is one random choice per block, not one per sentence.
  """

  def __init__(self, rng):
    self.blocks = [self.compose_block(rng) for _ in range(BLOCK_POOL_SIZE)]
    self.output_lines = [self.compose_output_line(rng) for _ in range(OUTPUT_LINE_POOL_SIZE)]

  def phrase(self, rng):
    return rng.choice(ZH_SENTENCES if rng.random() < 0.5 else EN_SENTENCES)

  def paragraph(self, rng):
    if rng.random() < 0.5:
      return ''.join(rng.choice(ZH_SENTENCES) for _ in range(rng.randint(2, 4)))
    return ' '.join(rng.choice(EN_SENTENCES) for _ in range(rng.randint(2, 4)))

  def compose_block(self, rng):
    kind = rng.random()
    if kind < 0.55:
      return self.paragraph(rng)
    if kind < 0.7:
      heading = rng.choice(ZH_TOPICS)
      items = '\n'.join(f'- {self.phrase(rng)}' for _ in range(rng.randint(2, 5)))
      return f'## {heading}\n\n{items}'
    if kind < 0.8:
      row = f'| {rng.choice(EN_TOPICS)} | 完成 | {self.phrase(rng)} |'
      return f'| 项目 | 状态 | 说明 |\n| --- | --- | --- |\n{row}'
    if kind < 0.92:
      language, code = rng.choice(CODE_SNIPPETS)
      return f'```{language}\n{code}\n```'
    return '\n'.join(f'{index}. **{rng.choice(EN_TOPICS)}**: {self.phrase(rng)}' for index in range(1, 4))

  def compose_output_line(self, rng):
    module = f'module_{rng.randint(1, 40):02d}'
    return f'src/{module}/file_{rng.randint(1, 300):03d}.rs:{rng.randint(1, 900)}: {self.phrase(rng)}'

  def title(self, rng):
    topic = rng.choice(ZH_TOPICS if rng.random() < 0.5 else EN_TOPICS)
    parts = [topic, f'#{rng.randint(1, 999)}']
    if rng.random() < 0.015:
      parts += [self.phrase(rng) for _ in range(rng.randint(10, 30))]
    return ' / '.join(parts)

  def tool_call(self, rng):
    name = rng.choice(TOOL_NAMES)
    module = f'module_{rng.randint(1, 40):02d}'
    if name == 'Read':
      tool_input = {'file_path': f'src/{module}/handler.rs'}
    elif name == 'Bash':
      tool_input = {'command': f'cargo test -p {module} --locked'}
    elif name == 'Grep':
      tool_input = {'pattern': 'TODO|FIXME', 'path': f'src/{module}'}
    elif name == 'Edit':
      tool_input = {'file_path': f'src/{module}/config.rs', 'old_string': 'retry: 3', 'new_string': 'retry: 5'}
    elif name == 'Glob':
      tool_input = {'pattern': f'src/{module}/**/*.rs'}
    else:
      raise ValueError(f'unknown tool name: {name}')
    return name, tool_input

  def tool_output(self, rng, lines):
    return '\n'.join(rng.choices(self.output_lines, k=lines))


def next_event(rng, text, timestamp, budget):
  kind = rng.random()
  if kind < 0.12:
    return {'type': 'user', 'content': text.paragraph(rng), 'timestamp': timestamp}
  if kind < 0.52:
    count = rng.randint(4, 40) if budget > LONG_EVENT_BUDGET else rng.randint(1, 4)
    body = '\n\n'.join(rng.choices(text.blocks, k=count))
    return {'type': 'assistant', 'message': {'content': [{'type': 'text', 'text': body}]}, 'timestamp': timestamp}
  if kind < 0.8:
    tool_name, tool_input = text.tool_call(rng)
    tool_use = {'type': 'tool_use', 'id': f'toolu_{rng.getrandbits(48):012x}', 'name': tool_name, 'input': tool_input}
    blocks = [{'type': 'text', 'text': text.phrase(rng)}, tool_use] if rng.random() < 0.5 else [tool_use]
    return {'type': 'assistant', 'message': {'content': blocks}, 'timestamp': timestamp}
  lines = rng.randint(1, 40) if budget > LONG_EVENT_BUDGET else rng.randint(1, 4)
  result = {
    'type': 'tool_result',
    'tool_use_id': f'toolu_{rng.getrandbits(48):012x}',
    'content': text.tool_output(rng, lines),
    'is_error': False,
  }
  return {'type': 'user', 'message': {'content': [result]}, 'timestamp': timestamp}


def encode_event(event):
  return (json.dumps(event, ensure_ascii=False) + '\n').encode('utf-8')


def filler_event(room, timestamp):
  """Return one user message whose line takes exactly room bytes, or None when room is too small for one."""
  empty = encode_event({'type': 'user', 'content': '', 'timestamp': timestamp})
  if room < len(empty):
    return None
  source = ' '.join(EN_SENTENCES)
  content = (source * (room // len(source) + 1))[:room - len(empty)]
  return {'type': 'user', 'content': content, 'timestamp': timestamp}


def opens_turn(event):
  """Return whether the event starts a turn: a plain user message, not a tool result."""
  return event['type'] == 'user' and 'content' in event


def master_done_event(rng, timestamp):
  """Return one turn separator in the production master_done shape that the renderer turns into a separator."""
  return {
    'type': 'master_done',
    'exit_code': 0,
    'still_thinking': False,
    'thinking_seconds': rng.randint(1, 120),
    'timestamp': timestamp,
  }


def write_event_log(path, target_bytes, rng, text, started_at, final_text):
  """Write events into one log of exactly target_bytes, ending with one final user message.

  Each turn ends with one master_done separator, and a plain user message closes the turn before it. The page's
  `tail(40)` snaps to a turn start through these separators, so a seeded log renders a bounded page like a
  production log instead of its whole history. Events are written while they fit in the room left for them. The
  last gap is filled by one filler message, so the log takes the planned size and not a size that the last event
  overshoots. Return (bytes, event count).
  """
  written = 0
  count = 0
  turn_open = False
  final_bytes = len(encode_event({'type': 'user', 'content': final_text, 'timestamp': event_stamp(started_at, 0)}))
  with open(path, 'wb') as log:
    while True:
      room = target_bytes - written - final_bytes
      if room < FILLER_THRESHOLD_BYTES:
        break
      event = next_event(rng, text, event_stamp(started_at, count), room)
      if opens_turn(event) and turn_open:
        line = encode_event(master_done_event(rng, event_stamp(started_at, count)))
        if len(line) > room:
          break
        log.write(line)
        written += len(line)
        count += 1
        turn_open = False
        room = target_bytes - written - final_bytes
      line = encode_event(event)
      if len(line) > room:
        break
      log.write(line)
      written += len(line)
      count += 1
      if not opens_turn(event):
        turn_open = True
    if turn_open:
      line = encode_event(master_done_event(rng, event_stamp(started_at, count)))
      if len(line) <= target_bytes - written - final_bytes:
        log.write(line)
        written += len(line)
        count += 1
    room = target_bytes - written - final_bytes
    if room > 0:
      filler = filler_event(room, event_stamp(started_at, count))
      if filler is not None:
        line = encode_event(filler)
        log.write(line)
        written += len(line)
        count += 1
    final = {'type': 'user', 'content': final_text, 'timestamp': event_stamp(started_at, count)}
    line = encode_event(final)
    log.write(line)
    written += len(line)
    count += 1
  return written, count


def prepare_output(output):
  os.makedirs(output, exist_ok=True)
  if os.listdir(output):
    raise SystemExit(f'output directory is not empty: {output}')


def generate(output, sessions, seed):
  prepare_output(output)
  rng = random.Random(seed)
  text = SyntheticText(rng)
  sizes = planned_sizes(sessions, rng)
  manifest_sessions = []
  total = 0
  for index in range(sessions):
    session_id = str(uuid.UUID(int=rng.getrandbits(128), version=4))
    session_dir = os.path.join(output, session_id)
    os.makedirs(os.path.join(session_dir, 'data'))
    started_at = FIRST_SESSION_TIME + datetime.timedelta(seconds=rng.randint(0, SESSION_START_SPREAD_SECONDS))
    final_text = f'Checkpoint {session_id}: {text.phrase(rng)}'
    log_path = os.path.join(session_dir, *EVENT_LOG_PARTS)
    log_bytes, event_count = write_event_log(log_path, sizes[index], rng, text, started_at, final_text)
    metadata = {
      'id': session_id,
      'name': text.title(rng),
      'status': 'active',
      'profile': 'manager',
      'backend': 'mock-backend',
      'created_at': stamp(started_at),
      'updated_at': stamp(started_at + datetime.timedelta(seconds=event_count)),
      'schema_version': 2,
      'has_unread': False,
      'starred': False,
    }
    with open(os.path.join(session_dir, METADATA_NAME), 'w', encoding='utf-8') as handle:
      json.dump(metadata, handle, ensure_ascii=False, indent=2)
    manifest_sessions.append({'id': session_id, 'event_log_bytes': log_bytes, 'last_message': final_text})
    total += log_bytes
  manifest = {'seed': seed, 'session_count': sessions, 'event_log_bytes': total, 'sessions': manifest_sessions}
  with open(os.path.join(output, MANIFEST_NAME), 'w', encoding='utf-8') as handle:
    json.dump(manifest, handle, ensure_ascii=False, indent=1)
  print(f'generated {sessions} sessions, {total} event-log bytes ({total / 2**30:.2f} GiB) in {output}')
  return 0


def auth_headers():
  value = os.environ.get('CHARLIEBOT_SERVE_AUTH_HEADER')
  if not value:
    return {}
  if ':' not in value:
    raise SystemExit('CHARLIEBOT_SERVE_AUTH_HEADER must read `Name: value`')
  name, _, token = value.partition(':')
  return {name.strip(): token.strip()}


def get_json(url, headers):
  request = urllib.request.Request(url, headers=headers)
  with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT_SECONDS) as response:
    return json.loads(response.read())


def check(output, base_url):
  with open(os.path.join(output, MANIFEST_NAME), encoding='utf-8') as handle:
    manifest = json.load(handle)
  base = base_url.rstrip('/')
  headers = auth_headers()
  failures = []
  try:
    listed = get_json(f'{base}/api/sessions/', headers)
  except (urllib.error.HTTPError, urllib.error.URLError) as error:
    print(f'MISMATCH session list: GET {base}/api/sessions/ failed: {error}', file=sys.stderr)
    return 1
  expected_count = manifest['session_count']
  if len(listed) != expected_count:
    failures.append(f'session count: manifest {expected_count}, server {len(listed)}')
  by_id = {entry['id']: entry for entry in manifest['sessions']}
  largest = max(manifest['sessions'], key=lambda entry: entry['event_log_bytes'])
  others = sorted(session_id for session_id in by_id if session_id != largest['id'])
  picked = random.Random(manifest['seed']).sample(others, min(2, len(others)))
  checked = [largest['id']] + picked
  for session_id in checked:
    expected_text = by_id[session_id]['last_message']
    url = f'{base}/api/sessions/{session_id}/events?before={NEWEST_PAGE_BEFORE}'
    try:
      messages = get_json(url, headers)['messages']
    except (urllib.error.HTTPError, urllib.error.URLError) as error:
      failures.append(f'session {session_id}: GET {url} failed: {error}')
      continue
    served_text = messages[-1].get('content') if messages else None
    if served_text != expected_text:
      failures.append(
        f'session {session_id}: manifest newest message {json.dumps(expected_text, ensure_ascii=False)}, '
        f'server newest message {json.dumps(served_text, ensure_ascii=False)}'
      )
  for failure in failures:
    print(f'MISMATCH {failure}', file=sys.stderr)
  if failures:
    return 1
  print(f'check passed: {expected_count} sessions; newest messages of {len(checked)} sessions match {MANIFEST_NAME}')
  return 0


def main(argv):
  parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
  commands = parser.add_subparsers(dest='command', required=True)
  generate_parser = commands.add_parser('generate', help='write a synthetic session home')
  generate_parser.add_argument('output')
  generate_parser.add_argument('--sessions', type=positive_int, default=PRODUCTION_SESSIONS)
  generate_parser.add_argument('--seed', type=int, default=DEFAULT_SEED)
  check_parser = commands.add_parser('check', help='read a served session home back against its manifest')
  check_parser.add_argument('output')
  check_parser.add_argument('base_url')
  args = parser.parse_args(argv)
  if args.command == 'generate':
    return generate(args.output, args.sessions, args.seed)
  if args.command == 'check':
    return check(args.output, args.base_url)
  raise ValueError(f'unknown command: {args.command}')


if __name__ == '__main__':
  sys.exit(main(sys.argv[1:]))
