import { useState } from 'react'
import { ChevronDown, ChevronRight, GitBranch } from 'lucide-react'
import { clsx } from 'clsx'
import type { ThreadMetadata, WorkerEvent } from '../../types'
import { StatusBadge } from '../common/Badge'
import { WorkerEventLog } from './WorkerEventLog'
import { useWebSocket } from '../../hooks/useWebSocket'
import { useThreadsStore } from '../../store/threads'
import { useDebugStore } from '../../store/debug'

interface Props {
  thread: ThreadMetadata
  sessionId: string
  onClick: () => void
  active: boolean
}

export function ThreadItem({ thread, onClick, active }: Props) {
  const [expanded, setExpanded] = useState(false)
  const { eventsByThread, appendEvent } = useThreadsStore()
  const debugMode = useDebugStore((s) => s.debugMode)
  const events = eventsByThread[thread.id] ?? []

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
    <div
      className={clsx(
        'rounded-lg border transition-colors cursor-pointer',
        active ? 'border-blue-500/40 bg-blue-500/5' : 'border-border bg-slate-800/30 hover:border-slate-600',
      )}
    >
      <div
        className="flex items-center gap-2 p-2.5"
        onClick={() => { setExpanded(!expanded); onClick() }}
      >
        <button
          onClick={(e) => { e.stopPropagation(); setExpanded(!expanded) }}
          className="text-slate-500 hover:text-slate-300"
        >
          {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        </button>
        <GitBranch size={12} className="text-slate-500 shrink-0" />
        <span className="text-xs text-slate-300 flex-1 truncate">{thread.description.slice(0, 60)}</span>
        <StatusBadge status={thread.status} />
      </div>

      {expanded && (
        <div className="border-t border-border/50 p-2 bg-slate-900/40">
          <p className="text-xs text-slate-600 mb-1 font-mono">{thread.branch_name}</p>
          {debugMode && (
            <div className="mb-2 rounded border border-orange-500/30 bg-orange-500/5 p-2 text-xs space-y-1">
              {thread.cli_command && (
                <div>
                  <span className="text-orange-400 font-semibold">CLI: </span>
                  <code className="text-slate-300 select-all break-all">{thread.cli_command}</code>
                </div>
              )}
              {thread.worktree_path && (
                <div>
                  <span className="text-orange-400 font-semibold">CWD: </span>
                  <span className="text-slate-400">{thread.worktree_path}</span>
                </div>
              )}
              <div>
                <span className="text-orange-400 font-semibold">Thread ID: </span>
                <span className="text-slate-400 font-mono">{thread.id}</span>
              </div>
              <div>
                <span className="text-orange-400 font-semibold">Task ID: </span>
                <span className="text-slate-400 font-mono">{thread.task_id}</span>
              </div>
              {thread.pid && (
                <div>
                  <span className="text-orange-400 font-semibold">PID: </span>
                  <span className="text-slate-400">{thread.pid}</span>
                </div>
              )}
            </div>
          )}
          <WorkerEventLog events={events} />
        </div>
      )}
    </div>
  )
}
