import { useEffect, useState } from 'react'
import { ChevronDown, ChevronRight, Plus } from 'lucide-react'
import { sessionsApi } from '../../api/sessions'
import { useSessionsStore } from '../../store/sessions'
import { SessionItem } from './SessionItem'
import type { SessionMetadata } from '../../types'

export function SessionList() {
  const { sessions, activeSessionId, setSessions, setActiveSession, addSession, removeSession, updateSession, markSessionRead } =
    useSessionsStore()
  const [creating, setCreating] = useState(false)
  const [archivedSessions, setArchivedSessions] = useState<SessionMetadata[]>([])
  const [showArchived, setShowArchived] = useState(false)

  useEffect(() => {
    sessionsApi.list().then(setSessions).catch(console.error)
    // Poll for unread status updates from background workers
    const interval = setInterval(() => {
      sessionsApi.list().then(setSessions).catch(console.error)
    }, 5000)
    return () => clearInterval(interval)
  }, [setSessions])

  useEffect(() => {
    if (showArchived) {
      sessionsApi.listArchived().then(setArchivedSessions).catch(console.error)
    }
  }, [showArchived])

  const handleCreate = async () => {
    if (creating) return
    setCreating(true)
    try {
      const session = await sessionsApi.create({})
      addSession(session)
      setActiveSession(session.id)
    } catch (e) {
      console.error('Failed to create session', e)
    } finally {
      setCreating(false)
    }
  }

  const handleRename = async (id: string, name: string) => {
    try {
      const updated = await sessionsApi.rename(id, name)
      updateSession(updated)
    } catch (e) {
      console.error('Failed to rename session', e)
    }
  }

  const handleArchive = async (id: string) => {
    try {
      const archived = await sessionsApi.archive(id)
      removeSession(id)
      setArchivedSessions((prev) => [archived, ...prev])
      setShowArchived(true)
    } catch (e) {
      console.error('Failed to archive session', e)
    }
  }

  const handleUnarchive = async (id: string) => {
    try {
      const restored = await sessionsApi.unarchive(id)
      addSession(restored)
      setArchivedSessions((prev) => prev.filter((s) => s.id !== id))
    } catch (e) {
      console.error('Failed to unarchive session', e)
    }
  }

  const active = sessions.filter((s) => s.status === 'active')

  return (
    <div className="flex flex-col">
      <div className="flex items-center justify-between px-3 py-2">
        <span className="text-xs font-semibold text-slate-500 uppercase tracking-wider">Sessions</span>
        <button
          onClick={handleCreate}
          disabled={creating}
          className="text-slate-500 hover:text-slate-300 transition-colors disabled:opacity-50"
          title="New session"
        >
          <Plus size={14} />
        </button>
      </div>

      <div className="space-y-0.5 px-1">
        {active.map((session) => (
          <SessionItem
            key={session.id}
            session={session}
            active={session.id === activeSessionId}
            hasUnread={session.has_unread && session.id !== activeSessionId}
            onClick={() => {
              setActiveSession(session.id)
              if (session.has_unread) {
                markSessionRead(session.id)
                sessionsApi.markRead(session.id).catch(console.error)
              }
            }}
            onRename={handleRename}
            onArchive={handleArchive}
          />
        ))}
        {active.length === 0 && (
          <p className="text-xs text-slate-600 px-2 py-1">No sessions yet</p>
        )}
      </div>

      {/* Archived section */}
      <div className="mt-2 border-t border-border pt-1">
        <button
          onClick={() => setShowArchived(!showArchived)}
          className="w-full flex items-center gap-1.5 px-3 py-1.5 text-xs font-semibold text-slate-500 uppercase tracking-wider hover:text-slate-400 transition-colors"
        >
          {showArchived ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          Archived
          {archivedSessions.length > 0 && (
            <span className="text-slate-600 normal-case font-normal">({archivedSessions.length})</span>
          )}
        </button>
        {showArchived && (
          <div className="space-y-0.5 px-1">
            {archivedSessions.map((session) => (
              <SessionItem
                key={session.id}
                session={session}
                active={session.id === activeSessionId}
                hasUnread={false}
                onClick={() => setActiveSession(session.id)}
                onRename={handleRename}
                onUnarchive={handleUnarchive}
              />
            ))}
            {archivedSessions.length === 0 && (
              <p className="text-xs text-slate-600 px-2 py-1">No archived sessions</p>
            )}
          </div>
        )}
      </div>
    </div>
  )
}
