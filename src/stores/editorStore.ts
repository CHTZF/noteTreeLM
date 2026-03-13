import { create } from 'zustand'

export type ViewMode = 'preview' | 'editor' | 'live'

interface EditorStore {
  currentPath: string | null
  content: string
  isDirty: boolean
  viewMode: ViewMode
  viewModes: Record<string, ViewMode>
  /** 外部寫入後通知 Editor 同步視圖：null = 無待更新 */
  pendingContent: string | null
  setCurrentPath: (path: string | null) => void
  setContent: (content: string) => void
  setDirty: (dirty: boolean) => void
  setViewMode: (mode: ViewMode) => void
  /** 外部（如 ChatPanel）寫入後呼叫，讓 Editor 同步 CM6 視圖並清除 dirty */
  applyExternalWrite: (content: string) => void
  clearPendingContent: () => void
}

export const useEditorStore = create<EditorStore>((set) => ({
  currentPath: null,
  content: '',
  isDirty: false,
  viewMode: 'preview',
  viewModes: {},
  pendingContent: null,

  setCurrentPath: (path) => set((s) => ({
    currentPath: path,
    isDirty: false,
    viewMode: path ? (s.viewModes[path] ?? 'preview') : 'preview',
  })),
  setContent: (content) => set({ content, isDirty: true }),
  setDirty: (dirty) => set({ isDirty: dirty }),
  setViewMode: (viewMode) => set((s) => ({
    viewMode,
    viewModes: s.currentPath
      ? { ...s.viewModes, [s.currentPath]: viewMode }
      : s.viewModes,
  })),
  applyExternalWrite: (content) => set({ pendingContent: content }),
  clearPendingContent: () => set({ pendingContent: null }),
}))
