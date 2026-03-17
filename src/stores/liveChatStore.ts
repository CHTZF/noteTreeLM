import { create } from 'zustand'

export interface LiveChatMessage {
  role: 'user' | 'assistant'
  content: string
  wikilinks?: { label: string; absPath: string }[]
}

interface LiveChatStore {
  messages: LiveChatMessage[]
  setMessages: (updater: (prev: LiveChatMessage[]) => LiveChatMessage[]) => void
  clearMessages: () => void
}

export const useLiveChatStore = create<LiveChatStore>(set => ({
  messages: [],
  setMessages: updater => set(s => ({ messages: updater(s.messages) })),
  clearMessages: () => set({ messages: [] }),
}))
