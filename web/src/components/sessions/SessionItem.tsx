import { Hash } from 'lucide-react'
import { clsx } from 'clsx'
import type { SessionMetadata } from '../../types'

interface Props {
  session: SessionMetadata
  active: boolean
  onClick: () => void
}

export function SessionItem({ session, active, onClick }: Props) {
  return (
    <button
      onClick={onClick}
      className={clsx(
        'w-full flex items-center gap-2 px-3 py-1.5 rounded text-sm text-left transition-colors',
        active
          ? 'bg-blue-600/20 text-white'
          : 'text-slate-400 hover:bg-slate-700/40 hover:text-slate-200',
      )}
    >
      <Hash size={14} className="shrink-0" />
      <span className="truncate">{session.name}</span>
    </button>
  )
}
