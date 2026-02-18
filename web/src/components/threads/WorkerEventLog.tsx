import { clsx } from 'clsx'
import type { WorkerEvent } from '../../types'

interface Props {
  events: WorkerEvent[]
}

const EVENT_COLORS: Record<string, string> = {
  thinking: 'text-slate-400 italic',
  file_write: 'text-green-400',
  error: 'text-red-400',
  complete: 'text-green-300 font-semibold',
  raw: 'text-slate-500',
  tool_use: 'text-blue-400',
  tool_result: 'text-slate-400',
  assistant: 'text-slate-300',
  catchup_complete: 'text-slate-600',
  ping: 'text-slate-700',
}

function EventRow({ event }: { event: WorkerEvent }) {
  const colorClass = EVENT_COLORS[event.type] ?? 'text-slate-400'

  let content = ''
  if (event.type === 'file_write') {
    content = `  ${event.path}${event.lines_added != null ? ` (+${event.lines_added} lines)` : ''}`
  } else if (event.content) {
    content = `  ${event.content.slice(0, 200)}`
  } else if (event.message) {
    content = `  ${event.message.slice(0, 200)}`
  }

  if (event.type === 'catchup_complete' || event.type === 'ping') return null

  return (
    <div className={clsx('font-mono text-xs leading-5', colorClass)}>
      <span className="text-slate-600 select-none">[{event.type}]</span>
      {content && <span>{content}</span>}
    </div>
  )
}

export function WorkerEventLog({ events }: Props) {
  if (events.length === 0) {
    return <p className="text-xs text-slate-600 py-2 text-center">No events yet</p>
  }

  return (
    <div className="overflow-y-auto max-h-80 space-y-0.5 px-1">
      {events.map((event, i) => (
        <EventRow key={i} event={event} />
      ))}
    </div>
  )
}
