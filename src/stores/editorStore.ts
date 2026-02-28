import { create } from 'zustand'

export type ViewMode = 'preview' | 'split' | 'editor'

interface EditorStore {
  currentPath: string | null
  content: string
  isDirty: boolean
  viewMode: ViewMode
  setCurrentPath: (path: string | null) => void
  setContent: (content: string) => void
  setDirty: (dirty: boolean) => void
  setViewMode: (mode: ViewMode) => void
}

export const useEditorStore = create<EditorStore>((set) => ({
  currentPath: null,
  content: '',
  isDirty: false,
  viewMode: 'preview',

  setCurrentPath: (path) => set({ currentPath: path, isDirty: false }),
  setContent: (content) => set({ content, isDirty: true }),
  setDirty: (dirty) => set({ isDirty: dirty }),
  setViewMode: (viewMode) => set({ viewMode }),
}))
