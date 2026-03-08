import { create } from 'zustand'
import { useEditorStore } from './editorStore'

export interface Tab {
  id: string
  path: string
}

interface TabStore {
  tabs: Tab[]
  activeTabId: string | null

  /** Open path in existing tab or create a new one. Also calls setCurrentPath. */
  openTab: (path: string) => void

  /** Close a tab by id; activates adjacent tab if it was active. */
  closeTab: (id: string) => void

  /** Switch active tab without pushing nav history. */
  activateTab: (id: string) => void

  /** Update the active tab's path (used by back/forward navigation). */
  setActiveTabPath: (path: string) => void

  /** Reorder tabs: move fromId to just before toId. */
  reorderTabs: (fromId: string, toId: string) => void
}

export const useTabStore = create<TabStore>((set, get) => ({
  tabs: [],
  activeTabId: null,

  openTab: (path) => {
    const { tabs } = get()
    const existing = tabs.find((t) => t.path === path)
    if (existing) {
      set({ activeTabId: existing.id })
    } else {
      const id = crypto.randomUUID()
      set((state) => ({ tabs: [...state.tabs, { id, path }], activeTabId: id }))
    }
    useEditorStore.getState().setCurrentPath(path)
  },

  closeTab: (id) => {
    const { tabs, activeTabId } = get()
    const idx = tabs.findIndex((t) => t.id === id)
    const nextTabs = tabs.filter((t) => t.id !== id)

    if (activeTabId === id) {
      const nextTab = nextTabs[Math.min(idx, nextTabs.length - 1)]
      set({ tabs: nextTabs, activeTabId: nextTab?.id ?? null })
      useEditorStore.getState().setCurrentPath(nextTab?.path ?? null)
    } else {
      set({ tabs: nextTabs })
    }
  },

  activateTab: (id) => {
    const { tabs } = get()
    const tab = tabs.find((t) => t.id === id)
    if (!tab) return
    set({ activeTabId: id })
    useEditorStore.getState().setCurrentPath(tab.path)
  },

  setActiveTabPath: (path) => {
    const { tabs, activeTabId } = get()
    if (!activeTabId) return
    set({ tabs: tabs.map((t) => (t.id === activeTabId ? { ...t, path } : t)) })
    useEditorStore.getState().setCurrentPath(path)
  },

  reorderTabs: (fromId, toId) => {
    set(state => {
      const tabs = [...state.tabs]
      const fromIdx = tabs.findIndex(t => t.id === fromId)
      if (fromIdx < 0) return state
      const [tab] = tabs.splice(fromIdx, 1)
      const toIdx = tabs.findIndex(t => t.id === toId)
      if (toIdx < 0) { tabs.push(tab); return { tabs } }
      tabs.splice(toIdx, 0, tab)
      return { tabs }
    })
  },
}))
