import { create } from 'zustand'

interface NavigationStore {
  history: string[]
  currentIndex: number
  push: (path: string) => void
  back: () => string | null
  forward: () => string | null
  canGoBack: boolean
  canGoForward: boolean
}

export const useNavigationStore = create<NavigationStore>((set, get) => ({
  history: [],
  currentIndex: -1,
  canGoBack: false,
  canGoForward: false,

  push: (path) => {
    const { history, currentIndex } = get()
    // 截斷 currentIndex 之後的歷史，再加入新路徑
    const newHistory = [...history.slice(0, currentIndex + 1), path]
    const newIndex = newHistory.length - 1
    set({
      history: newHistory,
      currentIndex: newIndex,
      canGoBack: newIndex > 0,
      canGoForward: false,
    })
  },

  back: () => {
    const { history, currentIndex } = get()
    if (currentIndex <= 0) return null
    const newIndex = currentIndex - 1
    set({
      currentIndex: newIndex,
      canGoBack: newIndex > 0,
      canGoForward: true,
    })
    return history[newIndex]
  },

  forward: () => {
    const { history, currentIndex } = get()
    if (currentIndex >= history.length - 1) return null
    const newIndex = currentIndex + 1
    set({
      currentIndex: newIndex,
      canGoBack: true,
      canGoForward: newIndex < history.length - 1,
    })
    return history[newIndex]
  },
}))
