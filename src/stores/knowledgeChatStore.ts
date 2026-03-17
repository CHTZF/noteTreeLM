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
  error?: string
}

interface KnowledgeChatStore {
  messages: KBMessage[]
  selectedSessionId: string | null
  setMessages: (updater: (prev: KBMessage[]) => KBMessage[]) => void
  setSelectedSessionId: (id: string | null) => void
  clearMessages: () => void
}

export const useKnowledgeChatStore = create<KnowledgeChatStore>(set => ({
  messages: [],
  selectedSessionId: null,
  setMessages: updater => set(s => ({ messages: updater(s.messages) })),
  setSelectedSessionId: id => set({ selectedSessionId: id }),
  clearMessages: () => set({ messages: [] }),
}))
