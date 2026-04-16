import { useState, useRef, useEffect, useCallback } from 'react'
import { useTranslation } from 'react-i18next'
import { invoke } from '@tauri-apps/api/core'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import { type IconDefinition } from '@fortawesome/fontawesome-svg-core'
import {
  faFolder, faFolderOpen, faFile, faFileLines, faFileImage, faFileCode,
  faFileAudio, faFileVideo, faFilePdf, faFileZipper,
  faTrash, faPen, faArrowUpRightFromSquare,
  faChevronRight,
  faPlus, faFolderPlus, faFolderMinus, faFileArrowUp,
  faArrowDownAZ, faArrowUpZA,
} from '@fortawesome/free-solid-svg-icons'
import { useVaultStore } from '../../stores/vaultStore'
import { useEditorStore } from '../../stores/editorStore'
import { useSettingsStore } from '../../stores/settingsStore'
import { FileTreeNode } from '../../types/models'
import { toast } from '../common/Toast'
import { ask } from '@tauri-apps/plugin-dialog'
import { open as openDialog } from '@tauri-apps/plugin-dialog'



// ── 檔案類型 icon + 顏色映射（VSCode Material Icon Theme 風格）────────────
// 顏色在深色/淺色主題下均清晰可辨
const FOLDER_COLOR  = '#dcb862'   // 溫暖金黃
const MD_COLOR      = '#519aba'   // 冷藍（Markdown）
const IMAGE_COLOR   = '#c586c0'   // 紫（PNG/JPG/GIF/WEBP/BMP）
const SVG_COLOR     = '#f0a500'   // 橘（SVG = 向量程式碼）
const CODE_COLOR    = '#56b6c2'   // 青（HTML/CSS/JS/TS 等）
const PDF_COLOR     = '#e06c75'   // 紅（PDF）
const AUDIO_COLOR   = '#98c379'   // 綠（MP3/WAV 等）
const VIDEO_COLOR   = '#61afef'   // 藍（MP4/MOV 等）
const ZIP_COLOR     = '#e5c07b'   // 淡金（壓縮檔）
const FILE_COLOR    = '#abb2bf'   // 灰（未知類型）

function getFileIcon(name: string, isImage?: boolean): { icon: IconDefinition; color: string } {
  const ext = name.toLowerCase().split('.').pop() ?? ''

  // 圖片資產（已標記為 isImage 或依副檔名判斷）
  if (isImage || ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'avif', 'ico', 'tiff', 'tif'].includes(ext))
    return { icon: faFileImage, color: IMAGE_COLOR }

  switch (ext) {
    case 'md': case 'markdown': case 'mdx':
      return { icon: faFileLines, color: MD_COLOR }
    case 'svg':
      return { icon: faFileCode, color: SVG_COLOR }
    case 'html': case 'htm': case 'css': case 'js': case 'ts': case 'tsx': case 'jsx':
    case 'json': case 'yaml': case 'yml': case 'toml': case 'xml': case 'rs': case 'py':
      return { icon: faFileCode, color: CODE_COLOR }
    case 'pdf':
      return { icon: faFilePdf, color: PDF_COLOR }
    case 'mp3': case 'wav': case 'ogg': case 'flac': case 'm4a': case 'aac':
      return { icon: faFileAudio, color: AUDIO_COLOR }
    case 'mp4': case 'mov': case 'avi': case 'mkv': case 'webm':
      return { icon: faFileVideo, color: VIDEO_COLOR }
    case 'zip': case 'rar': case '7z': case 'tar': case 'gz':
      return { icon: faFileZipper, color: ZIP_COLOR }
    default:
      return { icon: faFile, color: FILE_COLOR }
  }
}

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

// ── 依名稱排序並寫入 sort_orders（與拖曳排序相同機制）─────────────────────
async function applySortByName(nodes: FileTreeNode[], direction: 'asc' | 'desc') {
  const allOrders: Record<string, string[]> = {}
  function process(folderPath: string, children: FileTreeNode[]) {
    const sorted = [...children].sort((a, b) => {
      if (a.isFolder !== b.isFolder) return a.isFolder ? -1 : 1
      const cmp = a.name.localeCompare(b.name, 'zh-TW', { sensitivity: 'base' })
      return direction === 'asc' ? cmp : -cmp
    })
    allOrders[folderPath] = sorted.map(n => n.path)
    for (const node of sorted) {
      if (node.isFolder && node.children) process(node.path, node.children)
    }
  }
  process('', nodes)
  const store = useSettingsStore.getState()
  await store.savePersonal({ sort_orders: { ...(store.settings.sort_orders || {}), ...allOrders } })
  // Rebuild fileTree with the new sort_orders
  await useVaultStore.getState().loadNotes()
}

// ── 排序輔助 ──────────────────────────────────────────────────────────────
async function saveSortOrder(folderPath: string, orderedPaths: string[]) {
  const store = useSettingsStore.getState()
  const current = store.settings.sort_orders || {}
  await store.savePersonal({ sort_orders: { ...current, [folderPath]: orderedPaths } })
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
    await store.savePersonal({ sort_orders: newOrders })

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
    await store.savePersonal({ sort_orders: orders })

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
  onOpenNoteInNewTab?: (path: string) => void
}

export default function FileTree({ onOpenNote, onOpenNoteInNewTab }: FileTreeProps) {
  const { t } = useTranslation()
  const { fileTree, createNote, createFolder, importImage } = useVaultStore()
  const { currentPath } = useEditorStore()
  const { settings } = useSettingsStore()
  const [showNoteInput, setShowNoteInput] = useState(false)
  const [noteInputTitle, setNoteInputTitle] = useState('')
  const [showFolderInput, setShowFolderInput] = useState(false)
  const [folderInputName, setFolderInputName] = useState('')
  const treeRef = useRef<HTMLDivElement>(null)
  const noteInputRef = useRef<HTMLInputElement>(null)
  const folderInputRef = useRef<HTMLInputElement>(null)

  // 根容器登記為 drop target（path = ''）
  useEffect(() => {
    const el = treeRef.current
    if (el) {
      folderDropTargets.set(el, '')
      return () => { folderDropTargets.delete(el) }
    }
  }, [])

  const handleNewNote = () => {
    setShowFolderInput(false)
    setShowNoteInput(true); setNoteInputTitle('')
    setTimeout(() => noteInputRef.current?.focus(), 30)
  }
  const handleNewFolder = () => {
    setShowNoteInput(false)
    setShowFolderInput(true); setFolderInputName('')
    setTimeout(() => folderInputRef.current?.focus(), 30)
  }
  const handleNoteSubmit = async () => {
    const title = noteInputTitle.trim()
    setShowNoteInput(false); setNoteInputTitle('')
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

  const handleImportFile = async () => {
    const result = await openDialog({ multiple: true, directory: false })
    if (!result) return
    const files = Array.isArray(result) ? result : [result]
    for (const file of files) {
      try { await importImage(file as string) } catch {}
    }
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      {/* Header — icon buttons only */}
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'flex-end', padding: '4px 6px', borderBottom: '1px solid var(--color-border)', gap: '1px' }}>
        {([
          { icon: faPlus,        title: t('file_tree.new_note'),    onClick: handleNewNote },
          { icon: faFolderPlus,  title: t('file_tree.new_folder'),  onClick: handleNewFolder },
          { icon: faFileArrowUp, title: t('common.import'),         onClick: handleImportFile },
          { icon: faFolderMinus, title: '全部收合',                 onClick: () => window.dispatchEvent(new CustomEvent('filetree:collapse-all')) },
          { icon: faArrowDownAZ, title: '依名稱升序（A→Z）', onClick: () => applySortByName(fileTree, 'asc') },
          { icon: faArrowUpZA,   title: '依名稱降序（Z→A）', onClick: () => applySortByName(fileTree, 'desc') },
        ] as const).map(btn => (
          <button key={btn.title} onClick={btn.onClick} title={btn.title}
            style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--color-text-secondary)', fontSize: 'var(--icon-tree-btn-font)', width: 'var(--icon-tree-btn-size)', height: 'var(--icon-tree-btn-size)', display: 'flex', alignItems: 'center', justifyContent: 'center', borderRadius: '5px', opacity: 0.65, flexShrink: 0 }}
            onMouseEnter={(e) => { (e.currentTarget as HTMLElement).style.opacity = '1'; (e.currentTarget as HTMLElement).style.background = 'var(--color-bg-hover)' }}
            onMouseLeave={(e) => { (e.currentTarget as HTMLElement).style.opacity = '0.65'; (e.currentTarget as HTMLElement).style.background = 'none' }}>
            <FontAwesomeIcon icon={btn.icon} />
          </button>
        ))}
      </div>

      {showNoteInput && (
        <div style={{ padding: '6px 8px', borderBottom: '1px solid var(--color-border)' }}>
          <input ref={noteInputRef} value={noteInputTitle} onChange={(e) => setNoteInputTitle(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') handleNoteSubmit(); if (e.key === 'Escape') { setShowNoteInput(false); setNoteInputTitle('') } }}
            onBlur={handleNoteSubmit} placeholder={t('editor.untitled') + '…'}
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
            <p>{t('file_tree.empty')}</p><p style={{ marginTop: '8px' }}>{t('file_tree.empty_hint')}</p>
          </div>
        ) : fileTree.map((node) => (
          <TreeNode key={node.path} node={node} depth={0} currentPath={currentPath} vaultPath={settings.system_current_vault_path} onOpenNote={onOpenNote} onOpenNoteInNewTab={onOpenNoteInNewTab} />
        ))}
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
  onOpenNoteInNewTab?: (path: string) => void
}

function TreeNode({ node, depth, currentPath, vaultPath, onOpenNote, onOpenNoteInNewTab }: TreeNodeProps) {
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
  const { deleteNote, createFolder, renameFolder, deleteFolder, importImage, deleteAsset, renameNote, renameAsset } = useVaultStore()
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

  // 全部收合事件
  useEffect(() => {
    if (!node.isFolder) return
    const handler = () => setIsExpanded(false)
    window.addEventListener('filetree:collapse-all', handler)
    return () => window.removeEventListener('filetree:collapse-all', handler)
  }, [node.isFolder])

  const handleDeleteNote = async (e: React.MouseEvent) => {
    e.stopPropagation(); setMenuOpen(false)
    const confirmed = await ask(`確定要刪除「${node.path.split('/').pop() ?? node.name}」嗎？此操作無法復原。`, { title: '刪除', kind: 'warning' })
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
      const result = await renameNote(node.path, name)
      toast.success(`已重新命名為「${name}」`)
      window.dispatchEvent(new CustomEvent('note:renamed', { detail: { oldPath: node.path, newPath: result.new_path } }))
    } catch (e: any) { toast.error(e.message || '重新命名失敗') }
  }

  const handleDeleteAsset = async (e: React.MouseEvent) => {
    e.stopPropagation()
    const confirmed = await ask(`確定要刪除「${node.name}」嗎？此操作無法復原。`, { title: '刪除', kind: 'warning' })
    if (!confirmed) return
    try { await deleteAsset(node.path); toast.success(`已刪除「${node.name}」`) }
    catch (e: any) { toast.error(e.message || '刪除失敗') }
  }

  const handleRenameAsset = (e: React.MouseEvent) => {
    e.stopPropagation(); setMenuOpen(false)
    setRenameValue(node.name); setIsRenaming(true)
    setTimeout(() => renameInputRef.current?.focus(), 30)
  }
  const handleRenameAssetSubmit = async () => {
    setIsRenaming(false)
    const name = renameValue.trim()
    if (!name || name === node.name) return
    try {
      await renameAsset(node.path, name)
      toast.success(`已重新命名為「${name}」`)
    } catch (e: any) { toast.error(e.message || '重新命名失敗') }
  }

  // ── 筆記/資產節點共用拖曳邏輯 ───────────────────────────────────────────
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
          document.body.style.userSelect = 'none'
          document.body.style.webkitUserSelect = 'none'
          document.body.style.cursor = 'default'

          const ghost = document.createElement('div')
          ghost.textContent = srcName
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

  // ── 資產節點（圖片、PDF、音訊、影片等所有非筆記檔案）──────────────────
  if (!node.isFolder && !node.note) {
    const displayName = node.path.split('/').pop() ?? node.name
    return (
      <div
        data-item-path={node.path}
        data-item-parent={parentEncoded}
        onMouseDown={handleMouseDown}
        onClick={() => { if (dragJustEnded || isRenaming) return; onOpenNote(node.path) }}
        onContextMenu={(e) => { if (isRenaming) return; e.preventDefault(); e.stopPropagation(); window.getSelection()?.removeAllRanges(); setMenuX(e.clientX); setMenuY(e.clientY); setMenuOpen(true) }}
        onMouseEnter={() => setIsHovered(true)}
        onMouseLeave={() => setIsHovered(false)}
        style={{ display: 'flex', alignItems: 'center', gap: '6px', padding: '3px 8px 3px ' + (8 + depth * 16) + 'px', cursor: 'pointer', fontSize: '13px', color: 'var(--color-text-secondary)', background: isHovered ? 'var(--color-bg-hover)' : 'transparent', borderRadius: '6px', margin: '0 4px', transition: 'background 0.1s', userSelect: 'none' }}>
        <FontAwesomeIcon icon={getFileIcon(node.name, node.isImage).icon} style={{ fontSize: 'var(--icon-tree-item-font)', width: 'var(--icon-tree-item-font)', height: 'var(--icon-tree-item-font)', flexShrink: 0, color: getFileIcon(node.name, node.isImage).color }} />
        {isRenaming ? (
          <input
            ref={renameInputRef}
            value={renameValue}
            onChange={(e) => setRenameValue(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') { e.preventDefault(); handleRenameAssetSubmit() } if (e.key === 'Escape') { e.stopPropagation(); setIsRenaming(false) } }}
            onBlur={handleRenameAssetSubmit}
            onClick={(e) => e.stopPropagation()}
            onMouseDown={(e) => e.stopPropagation()}
            style={{ flex: 1, padding: '1px 4px', background: 'var(--color-bg-base)', border: '1px solid var(--color-accent)', borderRadius: '3px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none' }}
          />
        ) : (
          <span style={{ flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{displayName}</span>
        )}
        {menuOpen && (
          <div ref={menuRef} style={{ position: 'fixed', left: menuX, top: menuY - 15, background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '6px', boxShadow: '0 4px 16px rgba(0,0,0,0.35)', zIndex: 200, minWidth: '130px', overflow: 'hidden', padding: '4px 0' }}>
            {onOpenNoteInNewTab && <MenuItem icon={faArrowUpRightFromSquare} label="開啟新分頁" onClick={(e) => { e.stopPropagation(); setMenuOpen(false); onOpenNoteInNewTab(node.path) }} />}
            {onOpenNoteInNewTab && <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />}
            <MenuItem icon={faPen} label="重新命名" onClick={handleRenameAsset} />
            <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />
            <MenuItem icon={faTrash} label="刪除" danger onClick={handleDeleteAsset} />
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
        const file = await openDialog({ multiple: false })
        if (!file) return
        const filePath = typeof file === 'string' ? file : (file as any).path ?? String(file)
        await importImage(filePath, node.path); toast.success('檔案已匯入')
      } catch (e: any) { toast.error(e.message || '匯入檔案失敗') }
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
      <div ref={folderWrapperRef} style={{ outline: 'none', outlineOffset: '-2px', borderRadius: '6px', margin: '0 4px', transition: 'outline 0.08s' }}>
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
          <FontAwesomeIcon icon={faChevronRight} style={{ fontSize: 'var(--icon-tree-chevron)', width: 'var(--icon-tree-chevron)', height: 'var(--icon-tree-chevron)', flexShrink: 0, color: FOLDER_COLOR, transform: isExpanded ? 'rotate(90deg)' : 'rotate(0deg)', transition: 'transform 0.18s ease' }} />
          <FontAwesomeIcon icon={isExpanded ? faFolderOpen : faFolder} style={{ fontSize: 'var(--icon-tree-item-font)', width: 'var(--icon-tree-item-font)', height: 'var(--icon-tree-item-font)', flexShrink: 0, color: FOLDER_COLOR }} />
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
            <span style={{ flex: 1 }}>{node.name}</span>
          )}
        </div>
        {menuOpen && (
          <div ref={menuRef} style={{ position: 'fixed', left: menuX, top: menuY - 15, background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '6px', boxShadow: '0 4px 16px rgba(0,0,0,0.35)', zIndex: 200, minWidth: '148px', overflow: 'hidden', padding: '4px 0' }}>
            <MenuItem icon={faPlus} label="新增筆記" onClick={(e) => openSubInput('note', e)} />
            <MenuItem icon={faFolderPlus} label="新增子資料夾" onClick={(e) => openSubInput('folder', e)} />
            <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />
            <MenuItem icon={faFileImage} label="匯入檔案" onClick={handleImportImageToFolder} />
            <MenuItem icon={faPen} label="重新命名" onClick={handleRenameFolder} />
            <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />
            <MenuItem icon={faTrash} label="刪除資料夾" danger onClick={handleDeleteFolder} />
          </div>
        )}
        <div style={{ display: 'grid', gridTemplateRows: isExpanded ? '1fr' : '0fr', transition: 'grid-template-rows 0.18s ease', overflow: 'hidden' }}>
          <div style={{ minHeight: 0 }}>
            {showSubInput && (
              <div style={{ padding: '4px 8px 4px ' + (16 + depth * 16) + 'px' }}>
                <input ref={subInputRef} value={subInputName} onChange={(e) => setSubInputName(e.target.value)}
                  onKeyDown={(e) => { if (e.key === 'Enter') handleSubInputSubmit(); if (e.key === 'Escape') { setShowSubInput(false); setSubInputName('') } }}
                  onBlur={handleSubInputSubmit} placeholder={subInputType === 'folder' ? '子資料夾名稱…' : '筆記標題…'}
                  style={{ width: '100%', padding: '4px 8px', boxSizing: 'border-box', background: 'var(--color-bg-base)', border: '1px solid var(--color-accent)', borderRadius: '4px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none' }} />
              </div>
            )}
            {node.children?.map((child) => (
              <TreeNode key={child.path} node={child} depth={depth + 1} currentPath={currentPath} vaultPath={vaultPath} onOpenNote={onOpenNote} onOpenNoteInNewTab={onOpenNoteInNewTab} />
            ))}
          </div>
        </div>
      </div>
    )
  }

  // ── 筆記節點 ──────────────────────────────────────────────────────────────
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
        borderRadius: '6px', margin: '0 4px',
        transition: 'background 0.1s', userSelect: 'none',
      }}>
      <FontAwesomeIcon icon={getFileIcon(node.name).icon} style={{ fontSize: 'var(--icon-tree-item-font)', width: 'var(--icon-tree-item-font)', height: 'var(--icon-tree-item-font)', flexShrink: 0, color: getFileIcon(node.name).color }} />
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
          {node.path.split('/').pop() ?? node.name}
        </span>
      )}
      {menuOpen && (
        <div ref={menuRef} style={{ position: 'fixed', left: menuX, top: menuY - 15, background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '6px', boxShadow: '0 4px 16px rgba(0,0,0,0.35)', zIndex: 200, minWidth: '130px', overflow: 'hidden', padding: '4px 0' }}>
          {onOpenNoteInNewTab && <MenuItem icon={faArrowUpRightFromSquare} label="開啟新分頁" onClick={(e) => { e.stopPropagation(); setMenuOpen(false); onOpenNoteInNewTab(node.path) }} />}
          {onOpenNoteInNewTab && <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />}
          <MenuItem icon={faPen} label="重新命名" onClick={handleRenameNote} />
          <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />
          <MenuItem icon={faTrash} label="刪除" danger onClick={handleDeleteNote} />
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
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  icon: any; label: string; danger?: boolean; active?: boolean; onClick: (e: React.MouseEvent) => void
}
function MenuItem({ icon, label, danger, active, onClick }: MenuItemProps) {
  const [hovered, setHovered] = useState(false)
  const color = danger
    ? (hovered ? '#e06c75' : 'var(--color-text-secondary)')
    : active
    ? 'var(--color-accent)'
    : 'var(--color-text-secondary)'
  return (
    <button onClick={onClick} onMouseEnter={() => setHovered(true)} onMouseLeave={() => setHovered(false)}
      style={{ display: 'flex', alignItems: 'center', gap: '8px', width: '100%', padding: '6px 12px', fontSize: '13px', textAlign: 'left', color, background: hovered ? 'var(--color-bg-hover)' : 'transparent', cursor: 'pointer', fontWeight: active ? 600 : 400 }}>
      <FontAwesomeIcon icon={icon} style={{ fontSize: 'var(--icon-ctx-font)', width: 'var(--icon-ctx-font)', height: 'var(--icon-ctx-font)', flexShrink: 0 }} />{label}
    </button>
  )
}
