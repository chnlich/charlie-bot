import { create } from 'zustand'
import type { SessionMetadata } from '../types'

interface SessionsStore {
  sessions: SessionMetadata[]
  activeSessionId: string | null
  setSessions: (sessions: SessionMetadata[]) => void
  addSession: (session: SessionMetadata) => void
  updateSession: (updated: SessionMetadata) => void
  setActiveSession: (id: string | null) => void
}

export const useSessionsStore = create<SessionsStore>((set) => ({
  sessions: [],
  activeSessionId: null,
  setSessions: (sessions) => set({ sessions }),
  addSession: (session) =>
    set((s) => ({
      sessions: [session, ...s.sessions.filter((x) => x.id !== session.id)],
    })),
  updateSession: (updated) =>
    set((s) => ({
      sessions: s.sessions.map((x) => (x.id === updated.id ? updated : x)),
    })),
  setActiveSession: (id) => set({ activeSessionId: id }),
}))
