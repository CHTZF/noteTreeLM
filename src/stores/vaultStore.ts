import { create } from 'zustand'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { Note, FileTreeNode, RenameResult, TrashItem } from '../types/models'
import { useSettingsStore } from './settingsStore'
import { api } from '../lib/api'

let loadNotesInflight = false
let loadNotesPending  = false

// Lazy note content cache: path → content
// Populated on first read, updated on save, evicted on delete/rename.
const noteContentCache = new Map<string, string>()

/** Synchronous cache lookup — used by Editor to decide whether to clear first. */
export function getCachedNoteContent(path: string): string | undefined {
  return noteContentCache.get(path)
}

/** Evict one or more paths (e.g. after external rename/delete detected by watcher). */
export function evictNoteContentCache(...paths: string[]) {
  for (const p of paths) noteContentCache.delete(p)
}

interface VaultStore {
  notes: Note[]
  assets: string[]
  extraFolders: string[]
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
  extraFolders: [],
  fileTree: [],
  isScanning: false,
  scanCount: 0,

  loadNotes: async () => {
    if (loadNotesInflight) {
      loadNotesPending = true  // 完成後再跑一次，確保拿到最新資料
      return
    }
    loadNotesInflight = true
    loadNotesPending  = false
    try {
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
      set({ notes, assets, extraFolders, fileTree })
    } finally {
      loadNotesInflight = false
      if (loadNotesPending) {
        loadNotesPending = false
        get().loadNotes()  // 跑一次補上被 queue 的呼叫
      }
    }
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
    // Cache hit: return immediately without a network round-trip
    const cached = noteContentCache.get(path)
    if (cached !== undefined) {
      const meta = get().notes.find(n => n.path === path)
      return { path, content: cached, title: meta?.title ?? '', word_count: meta?.word_count ?? 0, created_at: meta?.created_at ?? 0, modified_at: meta?.modified_at ?? 0 } as Note
    }
    const vaultId = await invoke<string>('get_vault_uuid').catch(() => '')
    const note: Note = vaultId
      ? await api.readNote(vaultId, path) as Note
      : await invoke<Note>('read_note', { path })
    noteContentCache.set(path, note.content)
    return note
  },

  updateNote: async (path, content) => {
    const vaultId = await invoke<string>('get_vault_uuid')
    await api.updateNote(vaultId, { path, content })
    noteContentCache.set(path, content)
    set((state) => ({
      notes: state.notes.map((n) =>
        n.path === path ? { ...n, content, modified_at: Date.now() } : n
      ),
    }))
  },

  // 軟刪除：移至垃圾桶
  deleteNote: async (path) => {
    noteContentCache.delete(path)
    // Optimistic removal: update UI immediately before the API call returns
    set(state => {
      const notes = state.notes.filter(n => n.path !== path)
      const fileTree = get().buildFileTree(notes, state.extraFolders, state.assets, useSettingsStore.getState().settings.sort_orders)
      return { notes, fileTree }
    })
    await api.trashNote(path)
    get().loadNotes()  // background confirm — don't await
  },

  renameNote: async (path, newTitle) => {
    const { new_path } = await api.renameNote(path, newTitle)
    // Move cache entry to new path so reopening doesn't re-fetch
    const cached = noteContentCache.get(path)
    noteContentCache.delete(path)
    if (cached !== undefined) noteContentCache.set(new_path, cached)
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
    const prefix = folderPath.endsWith('/') ? folderPath : folderPath + '/'
    // Optimistic removal: remove all notes under this folder immediately
    set(state => {
      const notes = state.notes.filter(n => !n.path.startsWith(prefix) && n.path !== folderPath)
      const extraFolders = state.extraFolders.filter(f => !f.startsWith(prefix) && f !== folderPath)
      const fileTree = get().buildFileTree(notes, extraFolders, state.assets, useSettingsStore.getState().settings.sort_orders)
      return { notes, extraFolders, fileTree }
    })
    const { count } = await api.trashFolder(folderPath)
    get().loadNotes()  // background confirm — don't await
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
      loadTimer = setTimeout(() => { loadTimer = null; get().loadNotes() }, 1000)
    }

    let scanTimer: ReturnType<typeof setTimeout> | null = null
    const debouncedScan = () => {
      if (scanTimer) clearTimeout(scanTimer)
      scanTimer = setTimeout(async () => {
        scanTimer = null
        const vid = await invoke<string>('get_vault_uuid').catch(() => '')
        await api.scanVault(vid).catch(() => {})
        await get().loadNotes()
      }, 1000)
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
