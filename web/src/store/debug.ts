import { create } from 'zustand'

interface DebugStore {
  debugMode: boolean
  toggleDebug: () => void
}

export const useDebugStore = create<DebugStore>((set) => ({
  debugMode: localStorage.getItem('charliebot-debug') === 'true',
  toggleDebug: () =>
    set((s) => {
      const next = !s.debugMode
      localStorage.setItem('charliebot-debug', String(next))
      return { debugMode: next }
    }),
}))
