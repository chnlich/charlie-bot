import { create } from 'zustand'
import type { ChatMessage } from '../types'

interface ChatStore {
  // messages keyed by sessionId
  messagesBySession: Record<string, ChatMessage[]>
  streamingContent: Record<string, string> // sessionId -> partial assistant message
  addMessage: (sessionId: string, msg: ChatMessage) => void
  setMessages: (sessionId: string, msgs: ChatMessage[]) => void
  appendStream: (sessionId: string, chunk: string) => void
  clearStream: (sessionId: string) => string
}

export const useChatStore = create<ChatStore>((set, get) => ({
  messagesBySession: {},
  streamingContent: {},

  setMessages: (sessionId, msgs) =>
    set((s) => ({
      messagesBySession: { ...s.messagesBySession, [sessionId]: msgs },
    })),

  addMessage: (sessionId, msg) =>
    set((s) => ({
      messagesBySession: {
        ...s.messagesBySession,
        [sessionId]: [...(s.messagesBySession[sessionId] ?? []), msg],
      },
    })),

  appendStream: (sessionId, chunk) =>
    set((s) => ({
      streamingContent: {
        ...s.streamingContent,
        [sessionId]: (s.streamingContent[sessionId] ?? '') + chunk,
      },
    })),

  clearStream: (sessionId) => {
    const content = get().streamingContent[sessionId] ?? ''
    set((s) => ({
      streamingContent: { ...s.streamingContent, [sessionId]: '' },
    }))
    return content
  },
}))
