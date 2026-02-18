import { create } from 'zustand'
import type { ThreadMetadata, WorkerEvent } from '../types'

interface ThreadsStore {
  threadsBySession: Record<string, ThreadMetadata[]>
  eventsByThread: Record<string, WorkerEvent[]>
  activeThreadId: string | null
  setThreads: (sessionId: string, threads: ThreadMetadata[]) => void
  updateThread: (thread: ThreadMetadata) => void
  setEvents: (threadId: string, events: WorkerEvent[]) => void
  appendEvent: (threadId: string, event: WorkerEvent) => void
  setActiveThread: (id: string | null) => void
}

export const useThreadsStore = create<ThreadsStore>((set) => ({
  threadsBySession: {},
  eventsByThread: {},
  activeThreadId: null,

  setThreads: (sessionId, threads) =>
    set((s) => ({ threadsBySession: { ...s.threadsBySession, [sessionId]: threads } })),

  updateThread: (thread) =>
    set((s) => {
      const existing = s.threadsBySession[thread.session_id] ?? []
      const updated = existing.map((t) => (t.id === thread.id ? thread : t))
      if (!existing.find((t) => t.id === thread.id)) updated.unshift(thread)
      return { threadsBySession: { ...s.threadsBySession, [thread.session_id]: updated } }
    }),

  setEvents: (threadId, events) =>
    set((s) => ({ eventsByThread: { ...s.eventsByThread, [threadId]: events } })),

  appendEvent: (threadId, event) =>
    set((s) => ({
      eventsByThread: {
        ...s.eventsByThread,
        [threadId]: [...(s.eventsByThread[threadId] ?? []), event],
      },
    })),

  setActiveThread: (id) => set({ activeThreadId: id }),
}))
