// The Task & context dialog's Channel binding section for a thread-bound session: the platform origin the session
// stores, shown read-only. The binding is managed on the platform, so the section holds no control.
//
// Each platform names its stored origin field and the origin keys the section shows, in display order. A key the
// stored origin does not hold is not shown: a Feishu thread id is absent until the bot's first reply creates it.
const CHANNEL_BINDING_ORIGINS = [
  {
    platform: 'slack',
    origin: 'slack_origin',
    keys: [['team_id', 'Team'], ['channel_id', 'Channel'], ['thread_ts', 'Thread']],
  },
  {
    platform: 'discord',
    origin: 'discord_origin',
    keys: [['guild_id', 'Guild'], ['parent_channel_id', 'Channel'], ['thread_id', 'Thread']],
  },
  {
    platform: 'feishu',
    origin: 'feishu_origin',
    keys: [['tenant_key', 'Tenant'], ['chat_id', 'Chat'], ['root_id', 'Root message'], ['thread_id', 'Thread']],
  },
];

// One platform's section: its platform, the stored origin keys it holds, and the session's creation time.
function channelBindingSection(entry, origin, session) {
  const platform = Chat.platformLabel(entry.platform);
  const keyRows = entry.keys
    .filter(([key]) => origin[key] !== undefined && origin[key] !== null)
    .map(([key, label]) => Sidebar.taskContextField(
        label, `<span class="font-mono">${escapeHtml(String(origin[key]))}</span>`));
  const rows = [
    Sidebar.taskContextField('Platform', escapeHtml(platform)),
    ...keyRows,
    Sidebar.taskContextField('Created', escapeHtml(formatTimeLabel(session.created_at))),
  ].join('');
  return `<div class="space-y-3">
    <div class="text-xs font-semibold uppercase tracking-wide text-slate-400">Channel binding</div>
    <div class="space-y-3">${rows}</div>
    <div class="text-xs text-slate-400">Read-only: the binding is managed in ${escapeHtml(platform)}.</div>
  </div>`;
}

// The dialog's section loader for one session: the binding sections of the platform origins it stores, or '' for a
// session bound to no platform. A failed session read rejects, and the dialog shows the error.
async function loadChannelBindingSection(sessionId) {
  const res = await fetch('/api/sessions/' + sessionId);
  if (!res.ok) throw new Error(`session fetch failed: ${res.status}`);
  const session = await res.json();
  return CHANNEL_BINDING_ORIGINS
    .filter((entry) => session[entry.origin])
    .map((entry) => channelBindingSection(entry, session[entry.origin], session))
    .join('');
}

// A bound session's Task & context dialog shows the Channel binding section.
registerTaskContextSection(loadChannelBindingSection);
