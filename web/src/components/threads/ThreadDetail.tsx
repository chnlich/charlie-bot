import { useEffect } from 'react'
import { X } from 'lucide-react'
import type { ThreadMetadata, WorkerEvent } from '../../types'
import { threadsApi } from '../../api/threads'
import { useThreadsStore } from '../../store/threads'
import { useWebSocket } from '../../hooks/useWebSocket'
import { StatusBadge } from '../common/Badge'
import { WorkerEventLog } from './WorkerEventLog'
import { PlanReview } from '../plan/PlanReview'

interface Props {
  thread: ThreadMetadata
  sessionId: string
  onClose: () => void
}

export function ThreadDetail({ thread, sessionId, onClose }: Props) {
  const { eventsByThread, setEvents, appendEvent } = useThreadsStore()
  const events = eventsByThread[thread.id] ?? []

  // Load historical events
  useEffect(() => {
    threadsApi.getEvents(sessionId, thread.id).then((evs) => setEvents(thread.id, evs)).catch(console.error)
  }, [sessionId, thread.id, setEvents])

  // Subscribe to live events
  const isLive = thread.status === 'running' || thread.status === 'planning'
  useWebSocket(`/ws/threads/${thread.id}`, {
    enabled: isLive,
    onMessage: (data) => {
      const event = data as WorkerEvent
      if (event.type !== 'ping' && event.type !== 'catchup_complete') {
        appendEvent(thread.id, event)
      }
    },
  })

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center justify-between px-4 py-3 border-b border-border">
        <div className="flex items-center gap-2 min-w-0">
          <StatusBadge status={thread.status} />
          <span className="text-sm text-slate-300 truncate">{thread.description}</span>
        </div>
        <button onClick={onClose} className="text-slate-500 hover:text-white shrink-0 ml-2">
          <X size={16} />
        </button>
      </div>

      <div className="flex-1 overflow-y-auto p-4 space-y-4">
        <div className="text-xs text-slate-600 font-mono">{thread.branch_name}</div>

        {thread.status === 'awaiting_approval' && thread.id && (
          <PlanReview thread={thread} sessionId={sessionId} />
        )}

        <div>
          <h3 className="text-xs font-semibold text-slate-500 uppercase tracking-wider mb-2">Events</h3>
          <div className="bg-slate-900 rounded-lg p-3">
            <WorkerEventLog events={events} />
          </div>
        </div>
      </div>
    </div>
  )
}
