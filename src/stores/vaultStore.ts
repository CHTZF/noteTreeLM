import { create } from 'zustand'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { Note, FileTreeNode, RenameResult, TrashItem } from '../types/models'
import { useSettingsStore } from './settingsStore'

interface VaultStore {
  notes: Note[]
  assets: string[]
  fileTree: FileTreeNode[]
  isScanning: boolean
  scanCount: number
  // Actions
  loadNotes: () => Promise<void>
  scanVault: () => Promise<void>
  createNote: (title: string, folder?: string, content?: string) => Promise<Note>
  readNote: (path: string) => Promise<Note>
  updateNote: (path: string, content: string) => Promise<void>
  deleteNote: (path: string) => Promise<void>
  renameNote: (path: string, newTitle: string) => Promise<RenameResult>
  createFolder: (folderPath: string) => Promise<void>
  renameFolder: (folderPath: string, newName: string) => Promise<string>
  deleteFolder: (folderPath: string) => Promise<number>
  importImage: (sourcePath: string, folder?: string) => Promise<string>
  deleteAsset: (path: string) => Promise<void>
  renameAsset: (path: string, newName: string) => Promise<string>
  openPathExternally: (path: string) => Promise<void>
  // Trash
  listTrash: () => Promise<TrashItem[]>
  restoreTrashItem: (id: string, targetFolder: string) => Promise<string>
  deleteTrashItems: (ids: string[]) => Promise<void>
  buildFileTree: (notes: Note[], extraFolders?: string[], assets?: string[], sortOrders?: Record<string, string[]>) => FileTreeNode[]
  setupWatchers: () => Promise<() => void>
}

export const useVaultStore = create<VaultStore>((set, get) => ({
  notes: [],
  assets: [],
  fileTree: [],
  isScanning: false,
  scanCount: 0,

  loadNotes: async () => {
    const [notes, extraFolders, assets] = await Promise.all([
      invoke<Note[]>('list_notes', {}),
      invoke<string[]>('list_folders').catch(() => [] as string[]),
      invoke<string[]>('list_assets').catch(() => [] as string[]),
    ])
    const { settings } = useSettingsStore.getState()
    const fileTree = get().buildFileTree(notes, extraFolders, assets, settings.sort_orders)
    set({ notes, assets, fileTree })
  },

  scanVault: async () => {
    set({ isScanning: true, scanCount: 0 })
    try {
      const count = await invoke<number>('scan_vault')
      set({ scanCount: count })
      await get().loadNotes()
    } finally {
      set({ isScanning: false })
    }
  },

  createNote: async (title, folder, content) => {
    const note = await invoke<Note>('create_note', { title, folder, content })
    await get().loadNotes()
    return note
  },

  readNote: async (path) => {
    return invoke<Note>('read_note', { path })
  },

  updateNote: async (path, content) => {
    await invoke('update_note', { path, content })
    set((state) => ({
      notes: state.notes.map((n) =>
        n.path === path ? { ...n, content, modified_at: Date.now() } : n
      ),
    }))
  },

  // 軟刪除：移至垃圾桶
  deleteNote: async (path) => {
    await invoke('trash_note', { path })
    await get().loadNotes()
  },

  renameNote: async (path, newTitle) => {
    const result = await invoke<RenameResult>('rename_note', { path, newTitle })
    await get().loadNotes()
    return result
  },

  createFolder: async (folderPath) => {
    await invoke('create_folder', { folderPath })
    await get().loadNotes()
  },

  renameFolder: async (folderPath, newName) => {
    const newPath = await invoke<string>('rename_folder', { folderPath, newName })
    await get().loadNotes()
    return newPath
  },

  // 軟刪除資料夾：所有筆記移至垃圾桶，實體目錄刪除
  deleteFolder: async (folderPath) => {
    const count = await invoke<number>('trash_folder', { folderPath })
    await get().loadNotes()
    return count
  },

  importImage: async (sourcePath, folder) => {
    const relPath = await invoke<string>('import_image', { sourcePath, folder })
    await get().loadNotes()
    return relPath
  },

  deleteAsset: async (path) => {
    await invoke('delete_asset', { path })
    await get().loadNotes()
  },

  renameAsset: async (path, newName) => {
    const newPath = await invoke<string>('rename_asset', { path, newName })
    await get().loadNotes()
    return newPath
  },

  openPathExternally: async (path) => {
    await invoke('open_path_externally', { path })
  },

  // ── Trash ──────────────────────────────────────────────────
  listTrash: async () => {
    return invoke<TrashItem[]>('list_trash')
  },

  restoreTrashItem: async (id, targetFolder) => {
    const newPath = await invoke<string>('restore_trash_item', { id, targetFolder })
    // 復原後置頂於目標資料夾的排序
    const store = useSettingsStore.getState()
    const orders = { ...(store.settings.sort_orders || {}) }
    const existing = orders[targetFolder] || []
    orders[targetFolder] = [newPath, ...existing.filter(p => p !== newPath)]
    await store.save({ sort_orders: orders })
    await get().loadNotes()
    return newPath
  },

  deleteTrashItems: async (ids) => {
    await invoke('delete_trash_items', { ids })
  },

  buildFileTree: (notes, extraFolders = [], assets = [], sortOrders = {}) => {
    const root: FileTreeNode = { name: 'root', path: '', isFolder: true, children: [] }
    const folderMap = new Map<string, FileTreeNode>()
    folderMap.set('', root)

    const ensureFolder = (folderPath: string) => {
      if (folderMap.has(folderPath)) return
      const parts = folderPath.split('/')
      for (let i = 0; i < parts.length; i++) {
        const fp = parts.slice(0, i + 1).join('/')
        if (!folderMap.has(fp)) {
          const folderNode: FileTreeNode = {
            name: parts[i],
            path: fp,
            isFolder: true,
            children: [],
          }
          folderMap.set(fp, folderNode)
          const parentPath = parts.slice(0, i).join('/')
          const parent = folderMap.get(parentPath) || root
          parent.children = parent.children || []
          parent.children.push(folderNode)
        }
      }
    }

    for (const fp of extraFolders) ensureFolder(fp)
    for (const note of notes) {
      const parts = note.path.split('/')
      for (let i = 0; i < parts.length - 1; i++) ensureFolder(parts.slice(0, i + 1).join('/'))
    }
    for (const assetPath of assets) {
      const parts = assetPath.split('/')
      for (let i = 0; i < parts.length - 1; i++) ensureFolder(parts.slice(0, i + 1).join('/'))
    }

    for (const note of notes) {
      const parts = note.path.split('/')
      const parentPath = parts.slice(0, -1).join('/')
      const parent = folderMap.get(parentPath) || root
      parent.children = parent.children || []
      parent.children.push({ name: note.title, path: note.path, isFolder: false, note })
    }

    const IMAGE_EXTS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'avif', 'ico', 'tiff', 'tif', 'svg'])
    for (const assetPath of assets) {
      const parts = assetPath.split('/')
      const filename = parts[parts.length - 1]
      const ext = filename.split('.').pop()?.toLowerCase() ?? ''
      const parentPath = parts.slice(0, -1).join('/')
      const parent = folderMap.get(parentPath) || root
      parent.children = parent.children || []
      parent.children.push({ name: filename, path: assetPath, isFolder: false, isImage: IMAGE_EXTS.has(ext) })
    }

    const sortChildren = (node: FileTreeNode) => {
      if (!node.children) return
      const order = sortOrders[node.path]
      if (order && order.length > 0) {
        const byPath = new Map(node.children.map(c => [c.path, c]))
        const ordered: FileTreeNode[] = []
        for (const p of order) {
          const child = byPath.get(p)
          if (child) { ordered.push(child); byPath.delete(p) }
        }
        const remaining = Array.from(byPath.values()).sort((a, b) => {
          if (a.isFolder !== b.isFolder) return a.isFolder ? -1 : 1
          return a.name.localeCompare(b.name)
        })
        node.children = [...ordered, ...remaining]
      } else {
        node.children.sort((a, b) => {
          if (a.isFolder !== b.isFolder) return a.isFolder ? -1 : 1
          return a.name.localeCompare(b.name)
        })
      }
      node.children.forEach(sortChildren)
    }
    sortChildren(root)

    return root.children || []
  },

  setupWatchers: async () => {
    const unlisteners = await Promise.all([
      listen('vault:note-created', () => get().loadNotes()),
      listen('vault:note-updated', () => get().loadNotes()),
      listen('vault:note-deleted', () => get().loadNotes()),
      listen('vault:note-renamed', () => get().loadNotes()),
      // Agent 工具測試台寫入 commit 後觸發（create_note / update_note / create_folder）
      // 工具直接寫磁碟不更新 DB，需先 scan_vault 重建索引再 loadNotes
      listen('vault:changed', async () => {
        await invoke('scan_vault').catch(() => {})
        await get().loadNotes()
      }),
    ])
    return () => unlisteners.forEach((u) => u())
  },
}))
