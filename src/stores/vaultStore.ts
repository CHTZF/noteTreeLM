import { create } from 'zustand'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { Note, FileTreeNode, RenameResult, TrashItem } from '../types/models'
import { useSettingsStore } from './settingsStore'
import { api } from '../lib/api'

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
    const vaultId = await invoke<string>('get_vault_uuid').catch(() => '')
    const [notes, extraFolders, assets] = vaultId
      ? await Promise.all([
          api.listNotes(vaultId) as Promise<Note[]>,
          api.listFolders(vaultId).catch(() => [] as string[]),
          api.listAssets(vaultId).catch(() => [] as string[]),
        ])
      : [[] as Note[], [] as string[], [] as string[]]
    const { settings } = useSettingsStore.getState()
    const fileTree = get().buildFileTree(notes, extraFolders, assets, settings.sort_orders)
    set({ notes, assets, fileTree })
  },

  scanVault: async () => {
    set({ isScanning: true, scanCount: 0 })
    try {
      const vaultId = await invoke<string>('get_vault_uuid')
      const result = await api.scanVault(vaultId)
      set({ scanCount: result.indexed })
      await get().loadNotes()
    } finally {
      set({ isScanning: false })
    }
  },

  createNote: async (title, folder, content) => {
    const vaultId = await invoke<string>('get_vault_uuid')
    await api.createNote(vaultId, { title, folder: folder ?? null, content })
    await get().loadNotes()
    // Return newly created note from store
    const notes = get().notes
    return notes.find(n => n.title === title) ?? notes[0]
  },

  readNote: async (path) => {
    const vaultId = await invoke<string>('get_vault_uuid').catch(() => '')
    if (!vaultId) return invoke<Note>('read_note', { path })
    return api.readNote(vaultId, path) as Promise<Note>
  },

  updateNote: async (path, content) => {
    const vaultId = await invoke<string>('get_vault_uuid')
    await api.updateNote(vaultId, { path, content })
    set((state) => ({
      notes: state.notes.map((n) =>
        n.path === path ? { ...n, content, modified_at: Date.now() } : n
      ),
    }))
  },

  // 軟刪除：移至垃圾桶
  deleteNote: async (path) => {
    await api.trashNote(path)
    await get().loadNotes()
  },

  renameNote: async (path, newTitle) => {
    const { new_path } = await api.renameNote(path, newTitle)
    await get().loadNotes()
    return { new_path, updated_files: [] } as RenameResult
  },

  createFolder: async (folderPath) => {
    await api.createFolder(folderPath)
    await get().loadNotes()
  },

  renameFolder: async (folderPath, newName) => {
    const { new_folder_path } = await api.renameFolder(folderPath, newName)
    await get().loadNotes()
    return new_folder_path
  },

  // 軟刪除資料夾：所有筆記移至垃圾桶，實體目錄刪除
  deleteFolder: async (folderPath) => {
    const { count } = await api.trashFolder(folderPath)
    await get().loadNotes()
    return count
  },

  importImage: async (sourcePath, folder) => {
    const filename = sourcePath.split('/').pop() ?? sourcePath.split('\\').pop() ?? 'file'
    const contentBase64 = await invoke<string>('read_file_base64', { path: sourcePath })
    const { rel_path } = await api.importAsset(filename, contentBase64, folder ?? null, null)
    await get().loadNotes()
    return rel_path
  },

  deleteAsset: async (path) => {
    await api.deleteAsset(path)
    await get().loadNotes()
  },

  renameAsset: async (path, newName) => {
    const { new_path } = await api.renameAsset(path, newName)
    await get().loadNotes()
    return new_path
  },

  openPathExternally: async (path) => {
    await invoke('open_path_externally', { path })
  },

  // ── Trash ──────────────────────────────────────────────────────────────────
  listTrash: async () => {
    return api.listTrash() as Promise<TrashItem[]>
  },

  restoreTrashItem: async (id, targetFolder) => {
    const { new_path: newPath } = await api.restoreTrashItem(id, targetFolder)
    // 復原後置頂於目標資料夾的排序
    const store = useSettingsStore.getState()
    const orders = { ...(store.settings.sort_orders || {}) }
    const existing = orders[targetFolder] || []
    orders[targetFolder] = [newPath, ...existing.filter(p => p !== newPath)]
    await store.savePersonal({ sort_orders: orders })
    await get().loadNotes()
    return newPath
  },

  deleteTrashItems: async (ids) => {
    await api.deleteTrashItems(ids)
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
    // Debounced loadNotes: multiple rapid events (e.g. bulk import) collapse into one call
    let loadTimer: ReturnType<typeof setTimeout> | null = null
    const debouncedLoad = () => {
      if (loadTimer) clearTimeout(loadTimer)
      loadTimer = setTimeout(() => { loadTimer = null; get().loadNotes() }, 150)
    }

    let scanTimer: ReturnType<typeof setTimeout> | null = null
    const debouncedScan = () => {
      if (scanTimer) clearTimeout(scanTimer)
      scanTimer = setTimeout(async () => {
        scanTimer = null
        const vid = await invoke<string>('get_vault_uuid').catch(() => '')
        await api.scanVault(vid).catch(() => {})
        await get().loadNotes()
      }, 150)
    }

    const unlisteners = await Promise.all([
      listen('vault:note-created', debouncedLoad),
      listen('vault:note-updated', debouncedLoad),
      listen('vault:note-deleted', debouncedLoad),
      listen('vault:note-renamed', debouncedLoad),
      // Agent 工具測試台寫入 commit 後觸發（create_note / update_note / create_folder）
      // 工具直接寫磁碟不更新 DB，需先 scan_vault 重建索引再 loadNotes
      listen('vault:changed', debouncedScan),
    ])
    return () => {
      if (loadTimer) clearTimeout(loadTimer)
      if (scanTimer) clearTimeout(scanTimer)
      unlisteners.forEach((u) => u())
    }
  },
}))
