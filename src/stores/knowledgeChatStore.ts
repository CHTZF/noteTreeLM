import { create } from 'zustand'

export interface KnowledgeRef {
  path: string
  title: string
  excerpt: string
}

export interface KBMessage {
  id: string
  role: 'user' | 'assistant'
  content: string
  refs?: KnowledgeRef[]
  isStreaming?: boolean
  isCrossNote?: boolean
  error?: string
}

interface KnowledgeChatStore {
  messagesBySession: Record<string, KBMessage[]>
  selectedSessionId: string | null
  activeKbSessionId: string | null
  setMessages: (sessionKey: string, updater: (prev: KBMessage[]) => KBMessage[]) => void
  setSelectedSessionId: (id: string | null) => void
  setActiveKbSessionId: (id: string | null) => void
  clearMessages: (sessionKey: string) => void
}

export const useKnowledgeChatStore = create<KnowledgeChatStore>(set => ({
  messagesBySession: {},
  selectedSessionId: null,
  activeKbSessionId: null,
  setMessages: (sessionKey, updater) => set(s => ({
    messagesBySession: {
      ...s.messagesBySession,
      [sessionKey]: updater(s.messagesBySession[sessionKey] ?? []),
    },
  })),
  setSelectedSessionId: id => set({ selectedSessionId: id }),
  setActiveKbSessionId: id => set({ activeKbSessionId: id }),
  clearMessages: sessionKey => set(s => ({
    messagesBySession: { ...s.messagesBySession, [sessionKey]: [] },
  })),
}))
