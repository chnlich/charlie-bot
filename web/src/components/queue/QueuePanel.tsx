import { useEffect, useState } from 'react'
import { Trash2 } from 'lucide-react'
import { sessionsApi } from '../../api/sessions'
import type { Task, TaskQueue } from '../../types'
import { PriorityBadge, StatusBadge } from '../common/Badge'

interface Props {
  sessionId: string
}

export function QueuePanel({ sessionId }: Props) {
  const [queue, setQueue] = useState<TaskQueue | null>(null)

  const load = () =>
    sessionsApi.getQueue(sessionId).then(setQueue).catch(console.error)

  useEffect(() => {
    load()
    const interval = setInterval(load, 3000)
    return () => clearInterval(interval)
  }, [sessionId])

  const handleCancel = async (taskId: string) => {
    await sessionsApi.cancelTask(sessionId, taskId)
    load()
  }

  const pending = queue?.tasks.filter((t) => t.status === 'pending' || t.status === 'running') ?? []

  return (
    <div className="flex flex-col h-full">
      <div className="px-4 py-3 border-b border-border">
        <h2 className="text-sm font-semibold text-slate-300">Task Queue</h2>
        <p className="text-xs text-slate-500">{pending.length} active task{pending.length !== 1 ? 's' : ''}</p>
      </div>

      <div className="flex-1 overflow-y-auto p-3 space-y-2">
        {pending.length === 0 && (
          <p className="text-xs text-slate-600 text-center py-4">Queue is empty</p>
        )}
        {queue?.tasks.map((task) => (
          <TaskRow key={task.id} task={task} onCancel={handleCancel} />
        ))}
      </div>
    </div>
  )
}

function TaskRow({ task, onCancel }: { task: Task; onCancel: (id: string) => void }) {
  return (
    <div className="bg-slate-800/50 border border-border rounded-lg p-2.5 space-y-1.5">
      <div className="flex items-center gap-2">
        <PriorityBadge priority={task.priority} />
        <StatusBadge status={task.status} />
        {task.is_plan_mode && <span className="text-xs text-purple-400">plan</span>}
        {task.status === 'pending' && (
          <button
            onClick={() => onCancel(task.id)}
            className="ml-auto text-slate-600 hover:text-red-400 transition-colors"
            title="Cancel task"
          >
            <Trash2 size={12} />
          </button>
        )}
      </div>
      <p className="text-xs text-slate-400 truncate">{task.description}</p>
    </div>
  )
}
