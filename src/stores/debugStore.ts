import { create } from 'zustand'

export type DebugLevel = 'info' | 'warn' | 'error'

export interface DebugEntry {
  id: number
  timestamp: string   // HH:MM:SS.mmm
  category: string    // e.g. 'voice', 'transcription', 'system'
  level: DebugLevel
  message: string
}

let nextId = 1

function now(): string {
  const d = new Date()
  const hh = String(d.getHours()).padStart(2, '0')
  const mm = String(d.getMinutes()).padStart(2, '0')
  const ss = String(d.getSeconds()).padStart(2, '0')
  const ms = String(d.getMilliseconds()).padStart(3, '0')
  return `${hh}:${mm}:${ss}.${ms}`
}

interface DebugStore {
  entries: DebugEntry[]
  addLog: (category: string, level: DebugLevel, message: string) => void
  clear: () => void
}

export const useDebugStore = create<DebugStore>((set) => ({
  entries: [],

  addLog: (category, level, message) =>
    set((state) => ({
      entries: [
        ...state.entries,
        { id: nextId++, timestamp: now(), category, level, message },
      ].slice(-500), // keep last 500 entries to avoid unbounded growth
    })),

  clear: () => set({ entries: [] }),
}))
