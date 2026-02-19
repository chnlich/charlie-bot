import { useEffect, useState } from 'react'
import { Plus } from 'lucide-react'
import { sessionsApi } from '../../api/sessions'
import { useSessionsStore } from '../../store/sessions'
import { SessionItem } from './SessionItem'

export function SessionList() {
  const { sessions, activeSessionId, setSessions, setActiveSession, addSession, updateSession } =
    useSessionsStore()
  const [creating, setCreating] = useState(false)

  useEffect(() => {
    sessionsApi.list().then(setSessions).catch(console.error)
  }, [setSessions])

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
            onClick={() => setActiveSession(session.id)}
            onRename={handleRename}
          />
        ))}
        {active.length === 0 && (
          <p className="text-xs text-slate-600 px-2 py-1">No sessions yet</p>
        )}
      </div>
    </div>
  )
}
