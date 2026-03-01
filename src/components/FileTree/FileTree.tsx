import { useState, useRef, useEffect, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { useVaultStore } from '../../stores/vaultStore'
import { useEditorStore } from '../../stores/editorStore'
import { useSettingsStore } from '../../stores/settingsStore'
import { FileTreeNode } from '../../types/models'
import { toast } from '../common/Toast'
import { ask } from '@tauri-apps/plugin-dialog'
import { open as openDialog } from '@tauri-apps/plugin-dialog'
import { open as shellOpen } from '@tauri-apps/plugin-shell'

// ── Mouse-event DnD（相容 macOS WKWebView）────────────────────────────────
// 每個資料夾（含根容器）在 mount 時向這個 Map 登記
const folderDropTargets = new Map<Element, string>() // element → folder path ('' = root)

interface ActiveDrag {
  sourcePath: string
  sourceName: string
  sourceParent: string   // 來源所在的資料夾
  ghostEl: HTMLDivElement | null
  indicatorEl: HTMLDivElement | null
  dragging: boolean
  reorderMode: boolean   // true = 同資料夾排序；false = 跨資料夾移動
  insertBeforePath: string | null // reorder mode 用：插入在哪個 path 之前（null = 最後）
}
let activeDrag: ActiveDrag | null = null
let dragJustEnded = false // 防止 drag 結束後觸發 onClick

// ── 排序輔助 ──────────────────────────────────────────────────────────────
async function saveSortOrder(folderPath: string, orderedPaths: string[]) {
  const store = useSettingsStore.getState()
  const current = store.settings.sort_orders || {}
  await store.save({ sort_orders: { ...current, [folderPath]: orderedPaths } })
}

// 讀取某資料夾下所有 data-item-path 元素，按其螢幕位置排序
function getItemsInFolder(folderPath: string, excludePath?: string): Array<{ path: string; el: Element }> {
  const encoded = folderPath === '' ? '__root__' : folderPath
  const els = Array.from(document.querySelectorAll(`[data-item-parent="${encoded}"]`))
  return els
    .filter(el => el.getAttribute('data-item-path') !== excludePath)
    .map(el => ({ path: el.getAttribute('data-item-path')!, el }))
    .sort((a, b) => a.el.getBoundingClientRect().top - b.el.getBoundingClientRect().top)
}

function findInsertBeforePath(y: number, folderPath: string, excludePath: string): string | null {
  const items = getItemsInFolder(folderPath, excludePath)
  for (const { path, el } of items) {
    const rect = el.getBoundingClientRect()
    if (y < rect.top + rect.height / 2) return path
  }
  return null // 插入到最後
}

function positionIndicator(indicator: HTMLDivElement, y: number, folderPath: string, excludePath: string) {
  const items = getItemsInFolder(folderPath, excludePath)
  if (items.length === 0) {
    indicator.style.display = 'none'
    return
  }
  indicator.style.display = 'block'
  let lineY: number
  let lineLeft: number
  let lineWidth: number
  let found = false
  for (const { el } of items) {
    const rect = el.getBoundingClientRect()
    if (y < rect.top + rect.height / 2) {
      lineY = rect.top - 1
      lineLeft = rect.left
      lineWidth = rect.width
      found = true
      break
    }
  }
  if (!found) {
    const last = items[items.length - 1].el.getBoundingClientRect()
    lineY = last.bottom + 1
    lineLeft = last.left
    lineWidth = last.width
  }
  indicator.style.top = `${lineY!}px`
  indicator.style.left = `${lineLeft!}px`
  indicator.style.width = `${lineWidth!}px`
}

// ── 拖曳 Drop target 查找 ────────────────────────────────────────────────
function findFolderPath(x: number, y: number): string | undefined {
  const els = document.elementsFromPoint(x, y)
  for (const el of els) {
    if (folderDropTargets.has(el)) return folderDropTargets.get(el)!
  }
  return undefined
}

function clearHighlights() {
  folderDropTargets.forEach((_, el) => {
    ;(el as HTMLElement).style.outline = 'none'
  })
}

function highlightFolder(folderPath: string) {
  clearHighlights()
  for (const [el, path] of folderDropTargets) {
    if (path === folderPath) {
      ;(el as HTMLElement).style.outline = '2px solid var(--color-accent)'
      ;(el as HTMLElement).style.outlineOffset = '-2px'
      break
    }
  }
}

// ── 路徑前綴替換工具 ──────────────────────────────────────────────────────
function renamePrefix(path: string, oldPrefix: string, newPrefix: string): string {
  if (path === oldPrefix) return newPrefix
  if (path.startsWith(oldPrefix + '/')) return newPrefix + path.slice(oldPrefix.length)
  return path
}

// ── 執行資料夾移動 ────────────────────────────────────────────────────────
async function performMoveFolder(srcFolderPath: string, targetParent: string) {
  const folderName = srcFolderPath.split('/').pop()!
  const currentParent = srcFolderPath.includes('/')
    ? srcFolderPath.split('/').slice(0, -1).join('/')
    : ''
  if (currentParent === targetParent) return
  if (targetParent === srcFolderPath || targetParent.startsWith(srcFolderPath + '/')) {
    toast.error('不能將資料夾移動到自身或子資料夾')
    return
  }
  const newFolderPath = targetParent ? `${targetParent}/${folderName}` : folderName
  try {
    await invoke<string>('move_folder', { folderPath: srcFolderPath, newParent: targetParent })

    // 更新所有受影響的 sort_orders（路徑 prefix 替換）
    const store = useSettingsStore.getState()
    const orders = { ...(store.settings.sort_orders || {}) }
    const newOrders: Record<string, string[]> = {}
    for (const [key, val] of Object.entries(orders)) {
      const newKey = renamePrefix(key, srcFolderPath, newFolderPath)
      newOrders[newKey] = val.map(p => renamePrefix(p, srcFolderPath, newFolderPath))
    }
    // 從來源父資料夾的排序中移除舊路徑
    if (newOrders[currentParent]) {
      newOrders[currentParent] = newOrders[currentParent].filter(p => p !== srcFolderPath)
    }
    await store.save({ sort_orders: newOrders })

    await useVaultStore.getState().loadNotes()
    toast.success(`已移至「${targetParent || '根目錄'}」`)
  } catch (e: any) {
    toast.error(e.message || '移動資料夾失敗')
  }
}

// ── 執行筆記移動 ──────────────────────────────────────────────────────────
async function performMove(draggedPath: string, targetFolder: string) {
  const filename = draggedPath.split('/').pop()!
  const currentFolder = draggedPath.includes('/')
    ? draggedPath.split('/').slice(0, -1).join('/')
    : ''
  if (currentFolder === targetFolder) return

  const newPath = targetFolder ? `${targetFolder}/${filename}` : filename
  const { notes, deleteNote, loadNotes } = useVaultStore.getState()

  const hasConflict = notes.some(n => n.path === newPath)
  if (hasConflict) {
    const confirmed = await ask(
      `「${targetFolder || '根目錄'}」已有同名筆記「${filename}」，確定要取代嗎？`,
      { title: '名稱衝突', kind: 'warning' }
    )
    if (!confirmed) return
    try { await deleteNote(newPath) } catch {
      toast.error('無法刪除既有筆記，移動已取消'); return
    }
  }

  try {
    const resultPath = await invoke<string>('move_note', { oldPath: draggedPath, newFolder: targetFolder })

    // 更新排序：從來源資料夾移除，不加入目標（讓目標保持預設排序）
    const store = useSettingsStore.getState()
    const orders = { ...(store.settings.sort_orders || {}) }
    if (orders[currentFolder]) {
      orders[currentFolder] = orders[currentFolder].filter(p => p !== draggedPath)
    }
    await store.save({ sort_orders: orders })

    await loadNotes()
    const editorState = useEditorStore.getState()
    if (editorState.currentPath === draggedPath) editorState.setCurrentPath(resultPath)
    toast.success(`已移至「${targetFolder || '根目錄'}」`)
  } catch (e: any) {
    toast.error(e.message || '移動失敗')
  }
}

interface FileTreeProps {
  onOpenNote: (path: string) => void
}

export default function FileTree({ onOpenNote }: FileTreeProps) {
  const { fileTree, createNote, createFolder, importImage } = useVaultStore()
  const { currentPath } = useEditorStore()
  const { settings } = useSettingsStore()
  const [headerMenuOpen, setHeaderMenuOpen] = useState(false)
  const [showNoteInput, setShowNoteInput] = useState(false)
  const [noteInputTitle, setNoteInputTitle] = useState('')
  const [showFolderInput, setShowFolderInput] = useState(false)
  const [folderInputName, setFolderInputName] = useState('')
  const headerMenuRef = useRef<HTMLDivElement>(null)
  const treeRef = useRef<HTMLDivElement>(null)
  const noteInputRef = useRef<HTMLInputElement>(null)
  const folderInputRef = useRef<HTMLInputElement>(null)

  const vaultName = settings.vault_path
    ? settings.vault_path.replace(/\/+$/, '').split('/').pop() || 'Vault'
    : 'Vault'

  // 根容器登記為 drop target（path = ''）
  useEffect(() => {
    const el = treeRef.current
    if (el) {
      folderDropTargets.set(el, '')
      return () => { folderDropTargets.delete(el) }
    }
  }, [])

  // 點選 header 選單外部時關閉
  useEffect(() => {
    if (!headerMenuOpen) return
    const handler = (e: MouseEvent) => {
      if (headerMenuRef.current && !headerMenuRef.current.contains(e.target as Node)) {
        setHeaderMenuOpen(false)
      }
    }
    document.addEventListener('mousedown', handler)
    return () => document.removeEventListener('mousedown', handler)
  }, [headerMenuOpen])

  const handleNewNote = () => {
    setHeaderMenuOpen(false); setShowFolderInput(false)
    setShowNoteInput(true); setNoteInputTitle('')
    setTimeout(() => noteInputRef.current?.focus(), 30)
  }
  const handleNewFolder = () => {
    setHeaderMenuOpen(false); setShowNoteInput(false)
    setShowFolderInput(true); setFolderInputName('')
    setTimeout(() => folderInputRef.current?.focus(), 30)
  }
  const handleImportImage = async () => {
    setHeaderMenuOpen(false)
    try {
      const file = await openDialog({ multiple: false, filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp'] }] })
      if (!file) return
      const filePath = typeof file === 'string' ? file : (file as any).path ?? String(file)
      await importImage(filePath); toast.success('圖片已匯入 Vault')
    } catch (e: any) { toast.error(e.message || '匯入圖片失敗') }
  }
  const handleNoteSubmit = async () => {
    const title = noteInputTitle.trim(); setShowNoteInput(false); setNoteInputTitle('')
    if (!title) return
    try { const note = await createNote(title); onOpenNote(note.path); toast.success(`已建立「${title}」`) }
    catch (e: any) { toast.error(e.message || '建立失敗') }
  }
  const handleFolderSubmit = async () => {
    const name = folderInputName.trim(); setShowFolderInput(false); setFolderInputName('')
    if (!name) return
    try { await createFolder(name); toast.success(`已建立資料夾「${name}」`) }
    catch (e: any) { toast.error(e.message || '建立資料夾失敗') }
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      {/* Header */}
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '12px 12px 8px', borderBottom: '1px solid var(--color-border)' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '6px', overflow: 'hidden' }}>
          <span style={{ fontSize: '14px', flexShrink: 0 }}>📁</span>
          <span style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
            {vaultName}
          </span>
        </div>
        <div ref={headerMenuRef} style={{ position: 'relative', flexShrink: 0 }}>
          <button onClick={() => setHeaderMenuOpen((v) => !v)} title="新增…"
            style={{ color: 'var(--color-text-secondary)', fontSize: '11px', letterSpacing: '0.05em', padding: '2px 6px', opacity: 0.6 }}
            onMouseEnter={(e) => { (e.currentTarget as HTMLElement).style.opacity = '1' }}
            onMouseLeave={(e) => { (e.currentTarget as HTMLElement).style.opacity = '0.6' }}>•••</button>
          {headerMenuOpen && (
            <div style={{ position: 'absolute', top: '100%', right: 0, background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '6px', boxShadow: '0 4px 16px rgba(0,0,0,0.35)', zIndex: 200, minWidth: '148px', overflow: 'hidden', padding: '4px 0' }}>
              <MenuItem icon="📝" label="新增筆記" onClick={handleNewNote} />
              <MenuItem icon="📁" label="新增資料夾" onClick={handleNewFolder} />
              <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />
              <MenuItem icon="🖼" label="匯入圖片" onClick={handleImportImage} />
            </div>
          )}
        </div>
      </div>

      {showNoteInput && (
        <div style={{ padding: '6px 8px', borderBottom: '1px solid var(--color-border)' }}>
          <input ref={noteInputRef} value={noteInputTitle} onChange={(e) => setNoteInputTitle(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') handleNoteSubmit(); if (e.key === 'Escape') { setShowNoteInput(false); setNoteInputTitle('') } }}
            onBlur={handleNoteSubmit} placeholder="筆記標題…"
            style={{ width: '100%', padding: '5px 8px', boxSizing: 'border-box', background: 'var(--color-bg-base)', border: '1px solid var(--color-accent)', borderRadius: '4px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none' }} />
        </div>
      )}

      {showFolderInput && (
        <div style={{ padding: '6px 8px', borderBottom: '1px solid var(--color-border)' }}>
          <input ref={folderInputRef} value={folderInputName} onChange={(e) => setFolderInputName(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') handleFolderSubmit(); if (e.key === 'Escape') { setShowFolderInput(false); setFolderInputName('') } }}
            onBlur={handleFolderSubmit} placeholder="資料夾名稱…"
            style={{ width: '100%', padding: '5px 8px', boxSizing: 'border-box', background: 'var(--color-bg-base)', border: '1px solid var(--color-accent)', borderRadius: '4px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none' }} />
        </div>
      )}

      {/* Tree */}
      <div ref={treeRef} style={{ flex: 1, overflowY: 'auto', padding: '4px 0', outline: 'none', outlineOffset: '-2px', transition: 'outline 0.08s', borderRadius: '4px' }}>
        {fileTree.length === 0 ? (
          <div style={{ padding: '20px 12px', color: 'var(--color-text-muted)', fontSize: '13px', textAlign: 'center' }}>
            <p>還沒有筆記</p><p style={{ marginTop: '8px' }}>點擊 ••• 新增第一篇</p>
          </div>
        ) : (
          fileTree.map((node) => (
            <TreeNode key={node.path} node={node} depth={0} currentPath={currentPath} vaultPath={settings.vault_path} onOpenNote={onOpenNote} />
          ))
        )}
      </div>
    </div>
  )
}

interface TreeNodeProps {
  node: FileTreeNode
  depth: number
  currentPath: string | null
  vaultPath: string
  onOpenNote: (path: string) => void
}

function TreeNode({ node, depth, currentPath, vaultPath, onOpenNote }: TreeNodeProps) {
  const [isExpanded, setIsExpanded] = useState(true)
  const [isHovered, setIsHovered] = useState(false)
  const [menuOpen, setMenuOpen] = useState(false)
  const [showSubInput, setShowSubInput] = useState(false)
  const [subInputType, setSubInputType] = useState<'folder' | 'note'>('folder')
  const [subInputName, setSubInputName] = useState('')
  const menuRef = useRef<HTMLDivElement>(null)
  const subInputRef = useRef<HTMLInputElement>(null)
  const folderWrapperRef = useRef<HTMLDivElement>(null)
  const renameInputRef = useRef<HTMLInputElement>(null)
  const [isRenaming, setIsRenaming] = useState(false)
  const [renameValue, setRenameValue] = useState('')
  const [menuX, setMenuX] = useState(0)
  const [menuY, setMenuY] = useState(0)
  const { deleteNote, createFolder, renameFolder, deleteFolder, importImage, deleteAsset, renameNote } = useVaultStore()
  const { currentPath: editorPath, setCurrentPath, setContent } = useEditorStore()

  // 點選選單外部時關閉
  useEffect(() => {
    if (!menuOpen) return
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuOpen(false)
    }
    document.addEventListener('mousedown', handler)
    return () => document.removeEventListener('mousedown', handler)
  }, [menuOpen])

  // 資料夾登記為 drop target
  useEffect(() => {
    if (!node.isFolder) return
    const el = folderWrapperRef.current
    if (el) {
      folderDropTargets.set(el, node.path)
      return () => { folderDropTargets.delete(el) }
    }
  }, [node.isFolder, node.path])

  const handleDeleteNote = async (e: React.MouseEvent) => {
    e.stopPropagation(); setMenuOpen(false)
    const confirmed = await ask(`確定要刪除「${node.name}」嗎？此操作無法復原。`, { title: '刪除筆記', kind: 'warning' })
    if (!confirmed) return
    try {
      await deleteNote(node.path)
      if (editorPath === node.path) { setContent(''); setCurrentPath(null) }
      toast.success(`已刪除「${node.name}」`)
    } catch (e: any) { toast.error(e.message || '刪除失敗') }
  }

  const handleRenameNote = (e: React.MouseEvent) => {
    e.stopPropagation(); setMenuOpen(false)
    setRenameValue(node.name); setIsRenaming(true)
    setTimeout(() => renameInputRef.current?.focus(), 30)
  }
  const handleRenameNoteSubmit = async () => {
    setIsRenaming(false)
    const name = renameValue.trim()
    if (!name || name === node.name) return
    try {
      await renameNote(node.path, name)
      toast.success(`已重新命名為「${name}」`)
    } catch (e: any) { toast.error(e.message || '重新命名失敗') }
  }

  const handleDeleteAsset = async (e: React.MouseEvent) => {
    e.stopPropagation()
    const confirmed = await ask(`確定要刪除圖片「${node.name}」嗎？此操作無法復原。`, { title: '刪除圖片', kind: 'warning' })
    if (!confirmed) return
    try { await deleteAsset(node.path); toast.success(`已刪除「${node.name}」`) }
    catch (e: any) { toast.error(e.message || '刪除失敗') }
  }

  // ── 圖片節點 ──────────────────────────────────────────────
  if (node.isImage) {
    const parentEncoded = node.path.includes('/') ? node.path.split('/').slice(0, -1).join('/') : '__root__'
    return (
      <div
        data-item-path={node.path}
        data-item-parent={parentEncoded}
        onClick={async () => { try { await shellOpen(`${vaultPath}/${node.path}`) } catch { toast.error('無法開啟圖片') } }}
        onContextMenu={(e) => { if (isRenaming) return; e.preventDefault(); e.stopPropagation(); window.getSelection()?.removeAllRanges(); setMenuX(e.clientX); setMenuY(e.clientY); setMenuOpen(true) }}
        onMouseEnter={() => setIsHovered(true)}
        onMouseLeave={() => setIsHovered(false)}
        style={{ display: 'flex', alignItems: 'center', gap: '6px', padding: '3px 8px 3px ' + (8 + depth * 16) + 'px', cursor: 'pointer', fontSize: '13px', color: 'var(--color-text-secondary)', background: isHovered ? 'var(--color-bg-hover)' : 'transparent', borderRadius: '4px', margin: '0 4px', transition: 'background 0.1s', userSelect: 'none' }}>
        <span style={{ fontSize: '12px' }}>🖼</span>
        <span style={{ flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{node.name}</span>
        {menuOpen && (
          <div ref={menuRef} style={{ position: 'fixed', left: menuX, top: menuY - 15, background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '6px', boxShadow: '0 4px 16px rgba(0,0,0,0.35)', zIndex: 200, minWidth: '120px', overflow: 'hidden', padding: '4px 0' }}>
            <MenuItem icon="🗑" label="刪除圖片" danger onClick={handleDeleteAsset} />
          </div>
        )}
      </div>
    )
  }

  // ── 資料夾節點 ────────────────────────────────────────────
  if (node.isFolder) {
    const parentEncoded = node.path.includes('/') ? node.path.split('/').slice(0, -1).join('/') : '__root__'
    const srcParent = node.path.includes('/') ? node.path.split('/').slice(0, -1).join('/') : ''

    const handleFolderMouseDown = (e: React.MouseEvent) => {
      if (isRenaming) return
      if (e.button !== 0) { if (e.button === 2) e.preventDefault(); return }
      e.stopPropagation()
      const startX = e.clientX
      const startY = e.clientY
      const srcPath = node.path
      const srcName = node.name

      const onMove = (ev: MouseEvent) => {
        if (!activeDrag) {
          const dx = ev.clientX - startX
          const dy = ev.clientY - startY
          if (dx * dx + dy * dy > 25) {
            document.body.style.userSelect = 'none'
            document.body.style.webkitUserSelect = 'none'
            document.body.style.cursor = 'default'

            const ghost = document.createElement('div')
            ghost.textContent = `📁 ${srcName}`
            Object.assign(ghost.style, {
              position: 'fixed', left: `${ev.clientX + 14}px`, top: `${ev.clientY - 10}px`,
              pointerEvents: 'none', background: 'var(--color-bg-elevated)',
              border: '1px solid var(--color-accent)', borderRadius: '6px',
              padding: '4px 10px', fontSize: '12px', color: 'var(--color-text-primary)',
              boxShadow: '0 4px 12px rgba(0,0,0,0.4)', zIndex: '9999',
              whiteSpace: 'nowrap', maxWidth: '200px', overflow: 'hidden',
              textOverflow: 'ellipsis', userSelect: 'none',
            })
            document.body.appendChild(ghost)

            const indicator = document.createElement('div')
            Object.assign(indicator.style, {
              position: 'fixed', height: '2px', background: 'var(--color-accent)',
              pointerEvents: 'none', zIndex: '9998', borderRadius: '1px', display: 'none',
            })
            document.body.appendChild(indicator)

            activeDrag = { sourcePath: srcPath, sourceName: srcName, sourceParent: srcParent, ghostEl: ghost, indicatorEl: indicator, dragging: true, reorderMode: false, insertBeforePath: null }
          }
        }

        if (!activeDrag) return
        activeDrag.ghostEl!.style.left = `${ev.clientX + 14}px`
        activeDrag.ghostEl!.style.top = `${ev.clientY - 10}px`

        const targetFolder = findFolderPath(ev.clientX, ev.clientY)
        if (targetFolder === undefined) {
          clearHighlights()
          activeDrag.indicatorEl!.style.display = 'none'
          activeDrag.reorderMode = false
          return
        }
        // 不允許移動到自身或子資料夾
        if (targetFolder === srcPath || targetFolder.startsWith(srcPath + '/')) {
          clearHighlights()
          activeDrag.indicatorEl!.style.display = 'none'
          activeDrag.reorderMode = false
          return
        }

        if (targetFolder === srcParent) {
          clearHighlights()
          activeDrag.reorderMode = true
          activeDrag.insertBeforePath = findInsertBeforePath(ev.clientY, srcParent, srcPath)
          positionIndicator(activeDrag.indicatorEl!, ev.clientY, srcParent, srcPath)
        } else {
          activeDrag.indicatorEl!.style.display = 'none'
          activeDrag.reorderMode = false
          highlightFolder(targetFolder)
        }
      }

      const onUp = async (ev: MouseEvent) => {
        document.removeEventListener('mousemove', onMove)
        document.removeEventListener('mouseup', onUp)
        document.body.style.userSelect = ''
        document.body.style.webkitUserSelect = ''
        document.body.style.cursor = ''

        if (!activeDrag) return
        const drag = activeDrag
        activeDrag = null
        dragJustEnded = true
        setTimeout(() => { dragJustEnded = false }, 200)

        clearHighlights()
        if (drag.ghostEl) document.body.removeChild(drag.ghostEl)
        if (drag.indicatorEl) document.body.removeChild(drag.indicatorEl)
        if (!drag.dragging) return

        if (drag.reorderMode) {
          const items = getItemsInFolder(drag.sourceParent)
          const currentPaths = items.map(i => i.path)
          const filtered = currentPaths.filter(p => p !== drag.sourcePath)
          const insertIdx = drag.insertBeforePath ? filtered.indexOf(drag.insertBeforePath) : filtered.length
          const newOrder = [...filtered]
          newOrder.splice(insertIdx === -1 ? filtered.length : insertIdx, 0, drag.sourcePath)
          await saveSortOrder(drag.sourceParent, newOrder)
          await useVaultStore.getState().loadNotes()
        } else {
          const folderPath = findFolderPath(ev.clientX, ev.clientY)
          if (folderPath !== undefined && folderPath !== srcPath && !folderPath.startsWith(srcPath + '/')) {
            await performMoveFolder(srcPath, folderPath)
          }
        }
      }

      document.addEventListener('mousemove', onMove)
      document.addEventListener('mouseup', onUp)
    }

    const openSubInput = (type: 'folder' | 'note', e: React.MouseEvent) => {
      e.stopPropagation(); setMenuOpen(false); setSubInputType(type)
      setShowSubInput(true); setSubInputName(''); setIsExpanded(true)
      setTimeout(() => subInputRef.current?.focus(), 30)
    }
    const handleImportImageToFolder = async (e: React.MouseEvent) => {
      e.stopPropagation(); setMenuOpen(false)
      try {
        const file = await openDialog({ multiple: false, filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp'] }] })
        if (!file) return
        const filePath = typeof file === 'string' ? file : (file as any).path ?? String(file)
        await importImage(filePath, node.path); toast.success('圖片已匯入')
      } catch (e: any) { toast.error(e.message || '匯入圖片失敗') }
    }
    const handleDeleteFolder = async (e: React.MouseEvent) => {
      e.stopPropagation(); setMenuOpen(false)
      const totalCount = countDescendantNotes(node)
      const msg = totalCount > 0
        ? `確定要刪除資料夾「${node.name}」及其中的 ${totalCount} 篇筆記嗎？此操作無法復原。`
        : `確定要刪除空資料夾「${node.name}」嗎？`
      const confirmed = await ask(msg, { title: '刪除資料夾', kind: 'warning' })
      if (!confirmed) return
      try {
        await deleteFolder(node.path)
        const prefix = node.path + '/'
        if (editorPath && (editorPath === node.path || editorPath.startsWith(prefix))) { setContent(''); setCurrentPath(null) }
        toast.success(`已刪除資料夾「${node.name}」`)
      } catch (e: any) { toast.error(e.message || '刪除資料夾失敗') }
    }
    const handleRenameFolder = (e: React.MouseEvent) => {
      e.stopPropagation(); setMenuOpen(false)
      setRenameValue(node.name); setIsRenaming(true)
      setTimeout(() => renameInputRef.current?.focus(), 30)
    }
    const handleRenameFolderSubmit = async () => {
      setIsRenaming(false)
      const name = renameValue.trim()
      if (!name || name === node.name) return
      try {
        await renameFolder(node.path, name)
        toast.success(`已重新命名為「${name}」`)
      } catch (e: any) { toast.error(e.message || '重新命名失敗') }
    }

    const handleSubInputSubmit = async () => {
      const name = subInputName.trim(); setShowSubInput(false); setSubInputName('')
      if (!name) return
      try {
        if (subInputType === 'folder') {
          const folderPath = node.path ? `${node.path}/${name}` : name
          await createFolder(folderPath); toast.success(`已建立資料夾「${name}」`)
        } else {
          const { createNote } = useVaultStore.getState()
          const note = await createNote(name, node.path)
          onOpenNote(note.path); toast.success(`已建立「${name}」`)
        }
      } catch (e: any) { toast.error(e.message || '建立失敗') }
    }

    return (
      <div ref={folderWrapperRef} style={{ outline: 'none', outlineOffset: '-2px', borderRadius: '4px', margin: '0 4px', transition: 'outline 0.08s' }}>
        {/* 資料夾 header（data-item-path 供排序識別） */}
        <div
          data-item-path={node.path}
          data-item-parent={parentEncoded}
          onMouseDown={handleFolderMouseDown}
          onClick={() => { if (dragJustEnded || isRenaming) return; setIsExpanded(!isExpanded) }}
          onContextMenu={(e) => { if (isRenaming) return; e.preventDefault(); e.stopPropagation(); window.getSelection()?.removeAllRanges(); setMenuX(e.clientX); setMenuY(e.clientY); setMenuOpen(true) }}
          onMouseEnter={() => setIsHovered(true)}
          onMouseLeave={() => setIsHovered(false)}
          style={{ display: 'flex', alignItems: 'center', gap: '4px', padding: '3px 8px 3px ' + (8 + depth * 16) + 'px', cursor: 'pointer', fontSize: '13px', color: 'var(--color-text-secondary)', userSelect: 'none', position: 'relative' }}>
          <span style={{ fontSize: '10px' }}>{isExpanded ? '▼' : '▶'}</span>
          {isRenaming ? (
            <input
              ref={renameInputRef}
              value={renameValue}
              onChange={(e) => setRenameValue(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') { e.preventDefault(); handleRenameFolderSubmit() } if (e.key === 'Escape') { e.stopPropagation(); setIsRenaming(false) } }}
              onBlur={handleRenameFolderSubmit}
              onClick={(e) => e.stopPropagation()}
              onMouseDown={(e) => e.stopPropagation()}
              style={{ flex: 1, padding: '1px 4px', background: 'var(--color-bg-base)', border: '1px solid var(--color-accent)', borderRadius: '3px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none' }}
            />
          ) : (
            <span style={{ flex: 1 }}>📁 {node.name}</span>
          )}
        </div>
        {menuOpen && (
          <div ref={menuRef} style={{ position: 'fixed', left: menuX, top: menuY - 15, background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '6px', boxShadow: '0 4px 16px rgba(0,0,0,0.35)', zIndex: 200, minWidth: '148px', overflow: 'hidden', padding: '4px 0' }}>
            <MenuItem icon="📝" label="新增筆記" onClick={(e) => openSubInput('note', e)} />
            <MenuItem icon="📁" label="新增子資料夾" onClick={(e) => openSubInput('folder', e)} />
            <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />
            <MenuItem icon="🖼" label="匯入圖片" onClick={handleImportImageToFolder} />
            <MenuItem icon="✏️" label="重新命名" onClick={handleRenameFolder} />
            <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />
            <MenuItem icon="🗑" label="刪除資料夾" danger onClick={handleDeleteFolder} />
          </div>
        )}
        {isExpanded && (
          <>
            {showSubInput && (
              <div style={{ padding: '4px 8px 4px ' + (16 + depth * 16) + 'px' }}>
                <input ref={subInputRef} value={subInputName} onChange={(e) => setSubInputName(e.target.value)}
                  onKeyDown={(e) => { if (e.key === 'Enter') handleSubInputSubmit(); if (e.key === 'Escape') { setShowSubInput(false); setSubInputName('') } }}
                  onBlur={handleSubInputSubmit} placeholder={subInputType === 'folder' ? '子資料夾名稱…' : '筆記標題…'}
                  style={{ width: '100%', padding: '4px 8px', boxSizing: 'border-box', background: 'var(--color-bg-base)', border: '1px solid var(--color-accent)', borderRadius: '4px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none' }} />
              </div>
            )}
            {node.children?.map((child) => (
              <TreeNode key={child.path} node={child} depth={depth + 1} currentPath={currentPath} vaultPath={vaultPath} onOpenNote={onOpenNote} />
            ))}
          </>
        )}
      </div>
    )
  }

  // ── 筆記節點（mouse-event DnD）──────────────────────────────
  const isActive = node.path === currentPath
  const parentFolder = node.path.includes('/') ? node.path.split('/').slice(0, -1).join('/') : ''
  const parentEncoded = parentFolder === '' ? '__root__' : parentFolder

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    if (e.button !== 0) { if (e.button === 2) e.preventDefault(); return }
    e.stopPropagation()

    const startX = e.clientX
    const startY = e.clientY
    const srcPath = node.path
    const srcName = node.name
    const srcParent = parentFolder

    const onMove = (ev: MouseEvent) => {
      if (!activeDrag) {
        const dx = ev.clientX - startX
        const dy = ev.clientY - startY
        if (dx * dx + dy * dy > 25) {
          // 鎖定文字選取 + 游標
          document.body.style.userSelect = 'none'
          document.body.style.webkitUserSelect = 'none'
          document.body.style.cursor = 'default'

          const ghost = document.createElement('div')
          ghost.textContent = `📝 ${srcName}`
          Object.assign(ghost.style, {
            position: 'fixed', left: `${ev.clientX + 14}px`, top: `${ev.clientY - 10}px`,
            pointerEvents: 'none', background: 'var(--color-bg-elevated)',
            border: '1px solid var(--color-accent)', borderRadius: '6px',
            padding: '4px 10px', fontSize: '12px', color: 'var(--color-text-primary)',
            boxShadow: '0 4px 12px rgba(0,0,0,0.4)', zIndex: '9999',
            whiteSpace: 'nowrap', maxWidth: '200px', overflow: 'hidden',
            textOverflow: 'ellipsis', userSelect: 'none',
          })
          document.body.appendChild(ghost)

          const indicator = document.createElement('div')
          Object.assign(indicator.style, {
            position: 'fixed', height: '2px', background: 'var(--color-accent)',
            pointerEvents: 'none', zIndex: '9998', borderRadius: '1px', display: 'none',
          })
          document.body.appendChild(indicator)

          activeDrag = { sourcePath: srcPath, sourceName: srcName, sourceParent: srcParent, ghostEl: ghost, indicatorEl: indicator, dragging: true, reorderMode: false, insertBeforePath: null }
        }
      }

      if (!activeDrag) return
      activeDrag.ghostEl!.style.left = `${ev.clientX + 14}px`
      activeDrag.ghostEl!.style.top = `${ev.clientY - 10}px`

      const targetFolder = findFolderPath(ev.clientX, ev.clientY)
      if (targetFolder === undefined) {
        clearHighlights()
        activeDrag.indicatorEl!.style.display = 'none'
        activeDrag.reorderMode = false
        return
      }

      if (targetFolder === srcParent) {
        // 同資料夾：排序模式
        clearHighlights()
        activeDrag.reorderMode = true
        activeDrag.insertBeforePath = findInsertBeforePath(ev.clientY, srcParent, srcPath)
        positionIndicator(activeDrag.indicatorEl!, ev.clientY, srcParent, srcPath)
      } else {
        // 不同資料夾：移動模式
        activeDrag.indicatorEl!.style.display = 'none'
        activeDrag.reorderMode = false
        highlightFolder(targetFolder)
      }
    }

    const onUp = async (ev: MouseEvent) => {
      document.removeEventListener('mousemove', onMove)
      document.removeEventListener('mouseup', onUp)

      // 恢復文字選取 + 游標
      document.body.style.userSelect = ''
      document.body.style.webkitUserSelect = ''
      document.body.style.cursor = ''

      if (!activeDrag) return
      const drag = activeDrag
      activeDrag = null
      dragJustEnded = true
      setTimeout(() => { dragJustEnded = false }, 200)

      clearHighlights()
      if (drag.ghostEl) document.body.removeChild(drag.ghostEl)
      if (drag.indicatorEl) document.body.removeChild(drag.indicatorEl)

      if (!drag.dragging) return

      if (drag.reorderMode) {
        // 同資料夾排序
        const items = getItemsInFolder(drag.sourceParent)
        const currentPaths = items.map(i => i.path)
        const filtered = currentPaths.filter(p => p !== drag.sourcePath)
        const insertIdx = drag.insertBeforePath
          ? filtered.indexOf(drag.insertBeforePath)
          : filtered.length
        const newOrder = [...filtered]
        newOrder.splice(insertIdx === -1 ? filtered.length : insertIdx, 0, drag.sourcePath)
        await saveSortOrder(drag.sourceParent, newOrder)
        await useVaultStore.getState().loadNotes()
      } else {
        const folderPath = findFolderPath(ev.clientX, ev.clientY)
        if (folderPath !== undefined) {
          await performMove(drag.sourcePath, folderPath)
        }
      }
    }

    document.addEventListener('mousemove', onMove)
    document.addEventListener('mouseup', onUp)
  }, [node.path, node.name, parentFolder])

  return (
    <div
      data-item-path={node.path}
      data-item-parent={parentEncoded}
      onMouseDown={handleMouseDown}
      onClick={() => { if (dragJustEnded || isRenaming) return; onOpenNote(node.path) }}
      onContextMenu={(e) => { if (isRenaming) return; e.preventDefault(); e.stopPropagation(); window.getSelection()?.removeAllRanges(); setMenuX(e.clientX); setMenuY(e.clientY); setMenuOpen(true) }}
      onMouseEnter={() => setIsHovered(true)}
      onMouseLeave={() => setIsHovered(false)}
      style={{
        display: 'flex', alignItems: 'center', gap: '6px',
        padding: '3px 8px 3px ' + (8 + depth * 16) + 'px',
        cursor: 'pointer', fontSize: '13px',
        color: isActive ? 'var(--color-text-primary)' : 'var(--color-text-secondary)',
        background: isActive ? 'var(--color-accent-dim)' : isHovered ? 'var(--color-bg-hover)' : 'transparent',
        borderRadius: '4px', margin: '0 4px',
        transition: 'background 0.1s', userSelect: 'none',
      }}>
      <span style={{ fontSize: '12px' }}>📝</span>
      {isRenaming ? (
        <input
          ref={renameInputRef}
          value={renameValue}
          onChange={(e) => setRenameValue(e.target.value)}
          onKeyDown={(e) => { if (e.key === 'Enter') { e.preventDefault(); handleRenameNoteSubmit() } if (e.key === 'Escape') { e.stopPropagation(); setIsRenaming(false) } }}
          onBlur={handleRenameNoteSubmit}
          onClick={(e) => e.stopPropagation()}
          onMouseDown={(e) => e.stopPropagation()}
          style={{ flex: 1, padding: '1px 4px', background: 'var(--color-bg-base)', border: '1px solid var(--color-accent)', borderRadius: '3px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none' }}
        />
      ) : (
        <span style={{ flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', color: isActive ? 'var(--color-text-primary)' : 'var(--color-text-secondary)' }}>
          {node.name}
        </span>
      )}
      {menuOpen && (
        <div ref={menuRef} style={{ position: 'fixed', left: menuX, top: menuY - 15, background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '6px', boxShadow: '0 4px 16px rgba(0,0,0,0.35)', zIndex: 200, minWidth: '130px', overflow: 'hidden', padding: '4px 0' }}>
          <MenuItem icon="✏️" label="重新命名" onClick={handleRenameNote} />
          <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />
          <MenuItem icon="🗑" label="刪除筆記" danger onClick={handleDeleteNote} />
        </div>
      )}
    </div>
  )
}

function countDescendantNotes(node: FileTreeNode): number {
  if (!node.children) return 0
  return node.children.reduce((acc, child) => child.isFolder ? acc + countDescendantNotes(child) : acc + 1, 0)
}

interface MenuItemProps {
  icon: string; label: string; danger?: boolean; onClick: (e: React.MouseEvent) => void
}
function MenuItem({ icon, label, danger, onClick }: MenuItemProps) {
  const [hovered, setHovered] = useState(false)
  return (
    <button onClick={onClick} onMouseEnter={() => setHovered(true)} onMouseLeave={() => setHovered(false)}
      style={{ display: 'flex', alignItems: 'center', gap: '8px', width: '100%', padding: '6px 12px', fontSize: '13px', textAlign: 'left', color: danger ? (hovered ? '#e06c75' : 'var(--color-text-secondary)') : 'var(--color-text-secondary)', background: hovered ? 'var(--color-bg-hover)' : 'transparent', cursor: 'pointer' }}>
      <span style={{ fontSize: '12px' }}>{icon}</span>{label}
    </button>
  )
}
