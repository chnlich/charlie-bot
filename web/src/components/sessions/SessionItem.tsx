import { useState, useRef, useEffect } from 'react'
import { Hash } from 'lucide-react'
import { clsx } from 'clsx'
import type { SessionMetadata } from '../../types'

interface Props {
  session: SessionMetadata
  active: boolean
  hasUnread: boolean
  onClick: () => void
  onRename: (id: string, name: string) => Promise<void>
}

export function SessionItem({ session, active, hasUnread, onClick, onRename }: Props) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(session.name)
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (editing) {
      inputRef.current?.focus()
      inputRef.current?.select()
    }
  }, [editing])

  const startEditing = () => {
    setDraft(session.name)
    setEditing(true)
  }

  const commit = async () => {
    setEditing(false)
    const trimmed = draft.trim()
    if (trimmed && trimmed !== session.name) {
      await onRename(session.id, trimmed)
    }
  }

  const cancel = () => {
    setEditing(false)
    setDraft(session.name)
  }

  if (editing) {
    return (
      <div className="w-full flex items-center gap-2 px-3 py-1.5">
        <Hash size={14} className="shrink-0 text-slate-400" />
        <input
          ref={inputRef}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === 'Enter') commit()
            if (e.key === 'Escape') cancel()
          }}
          className="flex-1 min-w-0 bg-slate-700 text-sm text-white rounded px-1.5 py-0.5 outline-none ring-1 ring-blue-500"
        />
      </div>
    )
  }

  return (
    <button
      onClick={onClick}
      onDoubleClick={startEditing}
      className={clsx(
        'w-full flex items-center gap-2 px-3 py-1.5 rounded text-sm text-left transition-colors',
        active
          ? 'bg-blue-600/20 text-white'
          : 'text-slate-400 hover:bg-slate-700/40 hover:text-slate-200',
      )}
    >
      <Hash size={14} className="shrink-0" />
      <span className="truncate">{session.name}</span>
      {hasUnread && (
        <span className="relative ml-auto shrink-0 flex h-2 w-2">
          <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-yellow-400 opacity-75" />
          <span className="relative inline-flex rounded-full h-2 w-2 bg-yellow-400" />
        </span>
      )}
    </button>
  )
}
