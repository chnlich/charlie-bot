import { Bug, LayoutList, Users } from 'lucide-react'
import type { SessionMetadata } from '../../types'
import { useDebugStore } from '../../store/debug'

interface Props {
  session: SessionMetadata | null
  showThreads: boolean
  showQueue: boolean
  onToggleThreads: () => void
  onToggleQueue: () => void
}

export function Header({ session, showThreads, showQueue, onToggleThreads, onToggleQueue }: Props) {
  const { debugMode, toggleDebug } = useDebugStore()
  return (
    <div className="flex items-center justify-between px-4 py-3 border-b border-border bg-surface shrink-0">
      <div>
        <h1 className="text-sm font-semibold text-white">
          {session ? session.name : 'CharlieBot'}
        </h1>
        {session?.repo_path && (
          <p className="text-xs text-slate-500 truncate max-w-xs">{session.repo_path}</p>
        )}
      </div>
      <div className="flex items-center gap-1">
        <button
          onClick={toggleDebug}
          title="Debug mode"
          className={`p-1.5 rounded transition-colors ${debugMode ? 'text-orange-400 bg-orange-500/10' : 'text-slate-500 hover:text-slate-300'}`}
        >
          <Bug size={16} />
        </button>
        <button
          onClick={onToggleQueue}
          title="Task queue"
          className={`p-1.5 rounded transition-colors ${showQueue ? 'text-blue-400 bg-blue-500/10' : 'text-slate-500 hover:text-slate-300'}`}
        >
          <LayoutList size={16} />
        </button>
        <button
          onClick={onToggleThreads}
          title="Workers"
          className={`p-1.5 rounded transition-colors ${showThreads ? 'text-blue-400 bg-blue-500/10' : 'text-slate-500 hover:text-slate-300'}`}
        >
          <Users size={16} />
        </button>
      </div>
    </div>
  )
}
