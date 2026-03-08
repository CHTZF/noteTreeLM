import { useEffect, useState, useRef, useCallback, Fragment } from 'react'
import { listen } from '@tauri-apps/api/event'
import { invoke } from '@tauri-apps/api/core'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import { faGear, faBolt, faChevronLeft, faChevronRight, faSitemap, faFolderTree, faMagnifyingGlass, faBug } from '@fortawesome/free-solid-svg-icons'
import { useSettingsStore } from './stores/settingsStore'
import { useVaultStore } from './stores/vaultStore'
import { useGraphStore } from './stores/graphStore'
import { useEditorStore } from './stores/editorStore'
import { useNavigationStore } from './stores/navigationStore'
import type { Tab } from './stores/tabStore'
import TabBar, { SPECIAL_NAMES as TAB_SPECIAL_NAMES } from './components/TabBar/TabBar'

function getTabDisplayName(path: string): string {
  return TAB_SPECIAL_NAMES[path] ?? path.split('/').pop() ?? path
}
import GraphView from './components/Graph/GraphView'
import SearchPanel from './components/Search/SearchPanel'
import DebugPanel from './components/Debug/DebugPanel'
import ChatPanel from './components/Chat/ChatPanel'
import FileViewer from './components/FileViewer/FileViewer'
import PreviewPanel from './components/Editor/PreviewPanel'
import LiveChatPanel from './components/LiveChat/LiveChatPanel'
import QuickOpen from './components/QuickOpen/QuickOpen'
import SettingsModal from './components/Settings/SettingsModal'
import { AgentToolContent } from './components/AgentTools/AgentToolPanel'
import TrashPanel from './components/Trash/TrashPanel'
import Editor from './components/Editor/Editor'
import FileTree from './components/FileTree/FileTree'
import Onboarding from './components/Onboarding/Onboarding'
import Toast, { toast } from './components/common/Toast'
import './styles/App.css'

type RightPanelTab = 'chat' | 'live_chat'

const GRAPH_TAB = '__graph__'
const AGENT_TOOLS_TAB = '__agent_tools__'

// ─── Pane tree types ───────────────────────────────────────────────────────
interface PaneLeaf {
  kind: 'leaf'
  id: string
  tabs: Tab[]
  activeTabId: string | null
}
interface PaneGroup {
  kind: 'group'
  id: string
  direction: 'h' | 'v'
  children: PaneNode[]
}
type PaneNode = PaneLeaf | PaneGroup

// ─── Pane tree pure helpers ────────────────────────────────────────────────
function findLeaf(node: PaneNode, id: string): PaneLeaf | null {
  if (node.kind === 'leaf') return node.id === id ? node : null
  for (const c of node.children) { const f = findLeaf(c, id); if (f) return f }
  return null
}
function getAllLeaves(node: PaneNode): PaneLeaf[] {
  return node.kind === 'leaf' ? [node] : node.children.flatMap(getAllLeaves)
}
function mapLeaf(node: PaneNode, id: string, fn: (l: PaneLeaf) => PaneLeaf): PaneNode {
  if (node.kind === 'leaf') return node.id === id ? fn(node) : node
  return { ...node, children: node.children.map(c => mapLeaf(c, id, fn)) }
}
function splitLeaf(root: PaneNode, leafId: string, dir: 'h' | 'v', newLeaf: PaneLeaf): PaneNode {
  if (root.kind === 'leaf') {
    if (root.id !== leafId) return root
    return { kind: 'group', id: crypto.randomUUID(), direction: dir, children: [root, newLeaf] }
  }
  return { ...root, children: root.children.map(c => splitLeaf(c, leafId, dir, newLeaf)) }
}
function removeLeaf(root: PaneNode, leafId: string): PaneNode | null {
  if (root.kind === 'leaf') return root.id === leafId ? null : root
  const ch = root.children.map(c => removeLeaf(c, leafId)).filter((c): c is PaneNode => c !== null)
  if (ch.length === 0) return null
  if (ch.length === 1) return ch[0]
  return { ...root, children: ch }
}
function findLeafWithTab(node: PaneNode, tabId: string): PaneLeaf | null {
  if (node.kind === 'leaf') return node.tabs.some(t => t.id === tabId) ? node : null
  for (const c of node.children) { const f = findLeafWithTab(c, tabId); if (f) return f }
  return null
}
function reorderInLeaf(root: PaneNode, paneId: string, fromId: string, toId: string): PaneNode {
  return mapLeaf(root, paneId, leaf => {
    const tabs = [...leaf.tabs]
    const fi = tabs.findIndex(t => t.id === fromId)
    if (fi < 0) return leaf
    const [tab] = tabs.splice(fi, 1)
    const ti = tabs.findIndex(t => t.id === toId)
    if (ti < 0) tabs.push(tab); else tabs.splice(ti, 0, tab)
    return { ...leaf, tabs }
  })
}

// ─── App ───────────────────────────────────────────────────────────────────
export default function App() {
  const { load: loadSettings, settings } = useSettingsStore()
  const { scanVault, setupWatchers, readNote } = useVaultStore()
  const { load: loadGraph } = useGraphStore()
  const { currentPath } = useEditorStore()
  const { push: navPush, back: navBack, forward: navForward, canGoBack, canGoForward } = useNavigationStore()

  const [appReady, setAppReady] = useState(false)
  const [showOnboarding, setShowOnboarding] = useState(false)
  const [showSettings, setShowSettings] = useState(false)
  const [showTrash, setShowTrash] = useState(false)
  const [rightTab, setRightTab] = useState<RightPanelTab>('chat')
  const [showQuickOpen, setShowQuickOpen] = useState(false)
  const [sidebarWidth, setSidebarWidth] = useState(240)
  const [leftPanel, setLeftPanel] = useState<'files' | 'search' | 'debug' | null>('files')
  const [rightPanelWidth, setRightPanelWidth] = useState(320)
  const [liveChatActive, setLiveChatActive] = useState(false)

  // ─── Pane tree state ───────────────────────────────────────────────────
  const initialLeafIdRef = useRef('')
  const [paneRoot, setPaneRoot] = useState<PaneNode>(() => {
    const id = crypto.randomUUID()
    initialLeafIdRef.current = id
    return { kind: 'leaf', id, tabs: [], activeTabId: null }
  })
  const [focusedPaneId, setFocusedPaneId] = useState(() => initialLeafIdRef.current)
  const paneRootRef = useRef(paneRoot)
  paneRootRef.current = paneRoot
  const focusedPaneIdRef = useRef(focusedPaneId)
  focusedPaneIdRef.current = focusedPaneId

  // Content cache for non-focused panes (markdown preview)
  const [paneContents, setPaneContents] = useState<Record<string, string>>({})

  // ─── Drag state ────────────────────────────────────────────────────────
  const isDraggingLeft = useRef(false)
  const isDraggingRight = useRef(false)
  const dragStateRef = useRef<{
    tabId: string; startX: number; startY: number; active: boolean; paneId: string
  } | null>(null)
  const paneElemsRef = useRef<Map<string, HTMLElement>>(new Map())
  const [isDraggingTab, setIsDraggingTab] = useState(false)
  const [dragPos, setDragPos] = useState<{ x: number; y: number } | null>(null)
  const [dropZoneInfo, setDropZoneInfo] = useState<{ paneId: string; zone: 'right' | 'bottom' } | null>(null)
  const [dragOverTabId, setDragOverTabId] = useState<string | null>(null)

  // ─── Sync: focused pane active tab → editorStore.currentPath ──────────
  useEffect(() => {
    const leaf = findLeaf(paneRoot, focusedPaneId)
    const tab = leaf?.tabs.find(t => t.id === leaf.activeTabId)
    const newPath = tab?.path ?? null
    if (newPath !== useEditorStore.getState().currentPath) {
      useEditorStore.getState().setCurrentPath(newPath)
    }
  }, [paneRoot, focusedPaneId])

  // ─── Recovery: if focused pane removed, switch to first remaining ──────
  useEffect(() => {
    if (!findLeaf(paneRoot, focusedPaneId)) {
      const first = getAllLeaves(paneRoot)[0]
      if (first) setFocusedPaneId(first.id)
    }
  }, [paneRoot, focusedPaneId])

  // ─── Load content for non-focused panes ───────────────────────────────
  useEffect(() => {
    getAllLeaves(paneRoot)
      .filter(l => l.id !== focusedPaneId)
      .forEach(leaf => {
        const tab = leaf.tabs.find(t => t.id === leaf.activeTabId)
        if (!tab || !/\.(md|markdown|mdx)$/i.test(tab.path)) return
        readNote(tab.path)
          .then(note => setPaneContents(prev => ({ ...prev, [leaf.id]: note.content })))
          .catch(() => {})
      })
  }, [paneRoot, focusedPaneId])

  // ─── Sync editor content to non-focused panes showing same file ────────
  useEffect(() => {
    const { currentPath: editorPath, content } = useEditorStore.getState()
    if (!editorPath) return
    getAllLeaves(paneRoot).filter(l => l.id !== focusedPaneId).forEach(leaf => {
      const tab = leaf.tabs.find(t => t.id === leaf.activeTabId)
      if (tab?.path === editorPath) setPaneContents(prev => ({ ...prev, [leaf.id]: content }))
    })
  })

  // ─── Theme + font ──────────────────────────────────────────────────────
  useEffect(() => {
    document.documentElement.setAttribute('data-theme', settings.theme)
    if (settings.font_sans)
      document.documentElement.style.setProperty('--font-sans', settings.font_sans)
    else
      document.documentElement.style.removeProperty('--font-sans')
    document.documentElement.style.setProperty(
      '--font-mono',
      settings.font_mono || "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace"
    )
    document.documentElement.style.setProperty('--font-size-editor', `${settings.editor_font_size || 14}px`)
    document.documentElement.style.zoom = String((settings.ui_font_size || 14) / 14)
  }, [settings.theme, settings.font_sans, settings.font_mono, settings.editor_font_size, settings.ui_font_size])

  // ─── Init ──────────────────────────────────────────────────────────────
  useEffect(() => {
    const init = async () => {
      await loadSettings()
      const { settings } = useSettingsStore.getState()
      if (!settings.onboarding_done || !settings.vault_path) {
        setShowOnboarding(true)
      } else {
        setSidebarWidth(settings.sidebar_width)
        setRightPanelWidth(settings.graph_panel_width)
        await scanVault()
        await loadGraph()
        const lastNote = await invoke<string | null>('get_vault_last_note', { vaultPath: settings.vault_path }).catch(() => null)
        if (lastNote) {
          const tabId = crypto.randomUUID()
          const leafId = initialLeafIdRef.current
          setPaneRoot(prev => mapLeaf(prev, leafId, l => ({
            ...l, tabs: [{ id: tabId, path: lastNote }], activeTabId: tabId,
          })))
          useEditorStore.getState().setCurrentPath(lastNote)
          navPush(lastNote)
        }
      }
      setAppReady(true)
    }
    init()
  }, [])

  // ─── Save last open note ───────────────────────────────────────────────
  useEffect(() => {
    if (!currentPath || !settings.vault_path || currentPath === GRAPH_TAB) return
    invoke('set_vault_last_note', { vaultPath: settings.vault_path, notePath: currentPath }).catch(() => {})
  }, [currentPath, settings.vault_path])

  // ─── whisper-server status toasts ─────────────────────────────────────
  useEffect(() => {
    let loadingToastId: number | null = null
    let cancelled = false
    let unlisten: (() => void) | null = null
    listen<string>('whisper:stderr', (event) => {
      const line = event.payload
      if (line.startsWith('[server:error]')) {
        toast.error(line.replace('[server:error] ', ''), { duration: 0 })
      } else if (line.includes('等待模型載入') || line.includes('模型載入中')) {
        if (loadingToastId === null)
          loadingToastId = toast.info('whisper-server 載入模型中，請稍候…', { duration: 0 })
      } else if (line.includes('就緒')) {
        if (loadingToastId !== null) { toast.dismiss(loadingToastId); loadingToastId = null }
        toast.info('whisper-server 已就緒')
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [])

  // ─── llama-server status toasts ───────────────────────────────────────
  useEffect(() => {
    let loadingToastId: number | null = null
    let cancelled = false
    let unlisten: (() => void) | null = null
    listen<string>('llm:stderr', (event) => {
      const line = event.payload
      if (line.startsWith('[server:error]')) {
        toast.error(line.replace('[server:error] ', ''), { duration: 0 })
      } else if (line.includes('等待模型載入') || line.includes('模型載入中')) {
        if (loadingToastId === null)
          loadingToastId = toast.info('llama-server 載入模型中，請稍候…', { duration: 0 })
      } else if (line.includes('就緒')) {
        if (loadingToastId !== null) { toast.dismiss(loadingToastId); loadingToastId = null }
        toast.info('llama-server 已就緒')
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [])

  // ─── FileWatcher + graph sync ──────────────────────────────────────────
  useEffect(() => {
    if (!appReady || showOnboarding) return
    let cleanup: (() => void) | undefined
    const setup = async () => {
      const vaultCleanup = await setupWatchers()
      const graphListeners = await Promise.all(
        ['vault:note-created', 'vault:note-deleted', 'vault:note-renamed'].map(
          event => listen(event, () => loadGraph())
        )
      )
      const vaultChangedUnlisten = await listen<{ creates: string[]; updates: string[] }>(
        'vault:changed',
        e => {
          const { currentPath, applyExternalWrite } = useEditorStore.getState()
          if (currentPath && e.payload.updates.includes(currentPath)) {
            invoke<{ content: string }>('read_note', { path: currentPath })
              .then(note => applyExternalWrite(note.content))
              .catch(() => {})
          }
        }
      )
      const openNoteUnlisten = await listen<string>('ui:open_note', e => {
        if (e.payload) openNote(e.payload)
      })
      cleanup = () => {
        vaultCleanup()
        graphListeners.forEach(u => u())
        vaultChangedUnlisten()
        openNoteUnlisten()
      }
    }
    setup()
    return () => cleanup?.()
  }, [appReady, showOnboarding])

  // ─── Keyboard shortcuts ────────────────────────────────────────────────
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const meta = e.metaKey || e.ctrlKey
      if (meta && e.key === 'p' && !e.shiftKey) { e.preventDefault(); setShowQuickOpen(true) }
      if (meta && e.key === ',') { e.preventDefault(); setShowSettings(true) }
      if (meta && e.key === '[') {
        e.preventDefault()
        const prev = navBack()
        if (prev) setActiveTabPath(prev)
      }
      if (meta && e.key === ']') {
        e.preventDefault()
        const next = navForward()
        if (next) setActiveTabPath(next)
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [navBack, navForward])

  // ─── Open a note in the focused pane ──────────────────────────────────
  const openNote = useCallback((path: string) => {
    const root = paneRootRef.current
    const leafId = focusedPaneIdRef.current
    const leaf = findLeaf(root, leafId)
    if (!leaf) return
    const existing = leaf.tabs.find(t => t.path === path)
    if (existing) {
      setPaneRoot(prev => mapLeaf(prev, leafId, l => ({ ...l, activeTabId: existing.id })))
    } else {
      const id = crypto.randomUUID()
      setPaneRoot(prev => mapLeaf(prev, leafId, l => ({
        ...l, tabs: [...l.tabs, { id, path }], activeTabId: id,
      })))
    }
    useEditorStore.getState().setCurrentPath(path)
    navPush(path)
  }, [navPush])

  // ─── Update active tab path in focused pane (back/forward nav) ─────────
  const setActiveTabPath = useCallback((path: string) => {
    const leafId = focusedPaneIdRef.current
    setPaneRoot(prev => mapLeaf(prev, leafId, l => {
      if (!l.activeTabId) return l
      return { ...l, tabs: l.tabs.map(t => t.id === l.activeTabId ? { ...t, path } : t) }
    }))
    useEditorStore.getState().setCurrentPath(path)
  }, [])

  // ─── Close a tab in a specific pane ───────────────────────────────────
  const closeTabInPane = useCallback((paneId: string, tabId: string) => {
    const root = paneRootRef.current
    const leaf = findLeaf(root, paneId)
    if (!leaf) return
    const tabs = leaf.tabs.filter(t => t.id !== tabId)
    const idx = leaf.tabs.findIndex(t => t.id === tabId)
    const activeTabId = leaf.activeTabId === tabId
      ? (tabs[Math.min(idx, tabs.length - 1)]?.id ?? null)
      : leaf.activeTabId
    let newRoot = mapLeaf(root, paneId, l => ({ ...l, tabs, activeTabId }))
    let newFocusedId: string | null = null
    if (tabs.length === 0 && getAllLeaves(root).length > 1) {
      newRoot = removeLeaf(newRoot, paneId) ?? newRoot
      if (focusedPaneIdRef.current === paneId) {
        newFocusedId = getAllLeaves(newRoot)[0]?.id ?? null
      }
    }
    setPaneRoot(newRoot)
    if (newFocusedId) setFocusedPaneId(newFocusedId)
  }, [])

  // ─── Close a pane entirely (merge tabs to first remaining) ────────────
  const closePane = useCallback((paneId: string) => {
    const root = paneRootRef.current
    const leaves = getAllLeaves(root)
    if (leaves.length <= 1) return
    const closingLeaf = findLeaf(root, paneId)
    const targetLeaf = leaves.find(l => l.id !== paneId)
    if (!targetLeaf || !closingLeaf) return
    let newRoot = removeLeaf(root, paneId)
    if (!newRoot) return
    if (closingLeaf.tabs.length > 0) {
      newRoot = mapLeaf(newRoot, targetLeaf.id, l => ({
        ...l,
        tabs: [...l.tabs, ...closingLeaf.tabs],
        activeTabId: l.activeTabId ?? closingLeaf.activeTabId,
      }))
    }
    setPaneRoot(newRoot)
    if (focusedPaneIdRef.current === paneId) {
      setFocusedPaneId(targetLeaf.id)
      const tl = findLeaf(newRoot, targetLeaf.id)
      const tab = tl?.tabs.find(t => t.id === tl.activeTabId)
      if (tab) useEditorStore.getState().setCurrentPath(tab.path)
    }
  }, [])

  const openGraphTab = useCallback(() => openNote(GRAPH_TAB), [openNote])

  const handleOnboardingComplete = useCallback(async () => {
    setShowOnboarding(false)
    const { settings } = useSettingsStore.getState()
    setSidebarWidth(settings.sidebar_width)
    setRightPanelWidth(settings.graph_panel_width)
    await scanVault()
    await loadGraph()
  }, [])

  // ─── Sidebar resize ────────────────────────────────────────────────────
  const onLeftDividerMouseDown = () => { isDraggingLeft.current = true }
  const onRightDividerMouseDown = () => { isDraggingRight.current = true }
  useEffect(() => {
    const onMouseMove = (e: MouseEvent) => {
      if (isDraggingLeft.current) setSidebarWidth(Math.max(180, Math.min(400, e.clientX - 40)))
      if (isDraggingRight.current) setRightPanelWidth(Math.max(240, Math.min(480, window.innerWidth - e.clientX)))
    }
    const onMouseUp = () => { isDraggingLeft.current = false; isDraggingRight.current = false }
    window.addEventListener('mousemove', onMouseMove)
    window.addEventListener('mouseup', onMouseUp)
    return () => { window.removeEventListener('mousemove', onMouseMove); window.removeEventListener('mouseup', onMouseUp) }
  }, [])

  // ─── Tab drag ──────────────────────────────────────────────────────────
  useEffect(() => {
    const finishDrag = () => {
      dragStateRef.current = null
      setIsDraggingTab(false)
      setDragPos(null)
      setDropZoneInfo(null)
      setDragOverTabId(null)
      document.body.classList.remove('dragging-tab')
    }

    const onMouseMove = (e: MouseEvent) => {
      const ds = dragStateRef.current
      if (!ds) return
      if (!ds.active) {
        if (Math.abs(e.clientX - ds.startX) <= 4 && Math.abs(e.clientY - ds.startY) <= 4) return
        ds.active = true
        setIsDraggingTab(true)
        document.body.classList.add('dragging-tab')
      }
      setDragPos({ x: e.clientX, y: e.clientY })

      // Check each pane's content area for split zone
      for (const [pid, elem] of paneElemsRef.current) {
        const r = elem.getBoundingClientRect()
        if (e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom) {
          const relX = (e.clientX - r.left) / r.width
          const relY = (e.clientY - r.top) / r.height
          if (relX > 0.65) { setDropZoneInfo({ paneId: pid, zone: 'right' }); setDragOverTabId(null); return }
          if (relY > 0.65) { setDropZoneInfo({ paneId: pid, zone: 'bottom' }); setDragOverTabId(null); return }
          break // Inside a pane but not in split zone
        }
      }
      setDropZoneInfo(null)

      // Tab hover indicator
      const el = document.elementFromPoint(e.clientX, e.clientY)
      const tabEl = el?.closest('[data-tab-id]') as HTMLElement | null
      const overId = tabEl?.dataset.tabId ?? null
      setDragOverTabId(overId && overId !== ds.tabId ? overId : null)
    }

    const onMouseUp = (e: MouseEvent) => {
      const ds = dragStateRef.current
      if (!ds?.active) { finishDrag(); return }
      const root = paneRootRef.current

      // ── Re-compute split zone from DOM ──────────────────────────────
      let splitPaneId: string | null = null
      let splitDir: 'h' | 'v' | null = null
      for (const [pid, elem] of paneElemsRef.current) {
        const r = elem.getBoundingClientRect()
        if (e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom) {
          const relX = (e.clientX - r.left) / r.width
          const relY = (e.clientY - r.top) / r.height
          if (relX > 0.65) { splitPaneId = pid; splitDir = 'h' }
          else if (relY > 0.65) { splitPaneId = pid; splitDir = 'v' }
          break
        }
      }

      if (splitPaneId && splitDir) {
        const fromLeaf = findLeafWithTab(root, ds.tabId)
        if (fromLeaf) {
          const tab = fromLeaf.tabs.find(t => t.id === ds.tabId)!
          // Don't split a single-tab pane onto itself
          if (fromLeaf.id === splitPaneId && fromLeaf.tabs.length === 1) { finishDrag(); return }
          const newLeafId = crypto.randomUUID()
          const newLeaf: PaneLeaf = { kind: 'leaf', id: newLeafId, tabs: [tab], activeTabId: tab.id }
          let newRoot = mapLeaf(root, fromLeaf.id, l => {
            const tabs = l.tabs.filter(t => t.id !== ds.tabId)
            return { ...l, tabs, activeTabId: l.activeTabId === ds.tabId ? (tabs[0]?.id ?? null) : l.activeTabId }
          })
          newRoot = splitLeaf(newRoot, splitPaneId, splitDir, newLeaf)
          const srcAfter = findLeaf(newRoot, fromLeaf.id)
          if (srcAfter && srcAfter.tabs.length === 0 && getAllLeaves(newRoot).length > 1) {
            newRoot = removeLeaf(newRoot, fromLeaf.id) ?? newRoot
          }
          setPaneRoot(newRoot)
          setFocusedPaneId(newLeafId)
          useEditorStore.getState().setCurrentPath(tab.path)
        }
        finishDrag(); return
      }

      // ── Dropped on a specific tab (reorder or cross-pane move) ──────
      const el = document.elementFromPoint(e.clientX, e.clientY)
      const tabEl = el?.closest('[data-tab-id]') as HTMLElement | null
      const toTabId = tabEl?.dataset.tabId
      if (toTabId && toTabId !== ds.tabId) {
        const fromLeaf = findLeafWithTab(root, ds.tabId)
        const toLeaf = findLeafWithTab(root, toTabId)
        if (fromLeaf && toLeaf) {
          if (fromLeaf.id === toLeaf.id) {
            // Same pane — reorder
            setPaneRoot(prev => reorderInLeaf(prev, fromLeaf.id, ds.tabId, toTabId))
          } else {
            // Cross-pane move
            const tab = fromLeaf.tabs.find(t => t.id === ds.tabId)!
            let newRoot = mapLeaf(root, fromLeaf.id, l => {
              const tabs = l.tabs.filter(t => t.id !== ds.tabId)
              return { ...l, tabs, activeTabId: l.activeTabId === ds.tabId ? (tabs[0]?.id ?? null) : l.activeTabId }
            })
            newRoot = mapLeaf(newRoot, toLeaf.id, l => {
              const tabs = [...l.tabs]
              const ti = tabs.findIndex(t => t.id === toTabId)
              if (ti >= 0) tabs.splice(ti, 0, tab); else tabs.push(tab)
              return { ...l, tabs, activeTabId: tab.id }
            })
            const srcAfter = findLeaf(newRoot, fromLeaf.id)
            if (srcAfter && srcAfter.tabs.length === 0 && getAllLeaves(newRoot).length > 1) {
              newRoot = removeLeaf(newRoot, fromLeaf.id) ?? newRoot
            }
            setPaneRoot(newRoot)
            setFocusedPaneId(toLeaf.id)
            useEditorStore.getState().setCurrentPath(tab.path)
          }
        }
        finishDrag(); return
      }

      // ── Dropped on a tab bar area (move to that pane) ───────────────
      const tabBarEl = el?.closest('[data-pane-id]') as HTMLElement | null
      const targetPaneId = tabBarEl?.dataset.paneId
      if (targetPaneId && targetPaneId !== ds.paneId) {
        const fromLeaf = findLeafWithTab(root, ds.tabId)
        const toLeaf = findLeaf(root, targetPaneId)
        if (fromLeaf && toLeaf) {
          const tab = fromLeaf.tabs.find(t => t.id === ds.tabId)!
          let newRoot = mapLeaf(root, fromLeaf.id, l => {
            const tabs = l.tabs.filter(t => t.id !== ds.tabId)
            return { ...l, tabs, activeTabId: l.activeTabId === ds.tabId ? (tabs[0]?.id ?? null) : l.activeTabId }
          })
          newRoot = mapLeaf(newRoot, targetPaneId, l => ({
            ...l, tabs: [...l.tabs, tab], activeTabId: tab.id,
          }))
          const srcAfter = findLeaf(newRoot, fromLeaf.id)
          if (srcAfter && srcAfter.tabs.length === 0 && getAllLeaves(newRoot).length > 1) {
            newRoot = removeLeaf(newRoot, fromLeaf.id) ?? newRoot
          }
          setPaneRoot(newRoot)
          setFocusedPaneId(targetPaneId)
          useEditorStore.getState().setCurrentPath(tab.path)
        }
      }

      finishDrag()
    }

    window.addEventListener('mousemove', onMouseMove)
    window.addEventListener('mouseup', onMouseUp)
    return () => {
      window.removeEventListener('mousemove', onMouseMove)
      window.removeEventListener('mouseup', onMouseUp)
    }
  }, [])

  // ─── Render pane content ───────────────────────────────────────────────
  function renderPaneContent(leaf: PaneLeaf, isFocused: boolean, activePath: string | null) {
    if (!activePath) return (
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', height: '100%', color: 'var(--color-text-muted)', fontSize: '13px' }}>
        點擊頁籤開啟筆記
      </div>
    )
    if (activePath === GRAPH_TAB) return (
      <div style={{ flex: 1, overflow: 'hidden', display: 'flex' }}>
        <GraphView onOpenNote={openNote} />
      </div>
    )
    if (activePath === AGENT_TOOLS_TAB) return (
      <div style={{ flex: 1, minHeight: 0, overflow: 'hidden', display: 'flex', flexDirection: 'column' }}>
        <AgentToolContent />
      </div>
    )
    if (!/\.(md|markdown|mdx)$/i.test(activePath)) return <FileViewer path={activePath} />
    if (isFocused) return (
      <Editor
        canGoBack={canGoBack}
        canGoForward={canGoForward}
        onBack={() => { const p = navBack(); if (p) setActiveTabPath(p) }}
        onForward={() => { const p = navForward(); if (p) setActiveTabPath(p) }}
        onOpenNote={openNote}
      />
    )
    return (
      <PreviewPanel
        content={paneContents[leaf.id] ?? ''}
        onWikilinkClick={title => {
          const note = useVaultStore.getState().notes.find(n => n.title === title)
          if (note) openNote(note.path)
        }}
      />
    )
  }

  // ─── Render pane tree ──────────────────────────────────────────────────
  function renderPaneNode(node: PaneNode): React.ReactNode {
    if (node.kind === 'group') {
      return (
        <div className={`pane-area${node.direction === 'v' ? ' pane-area-v' : ''}`}>
          {node.children.map((child, i) => (
            <Fragment key={child.id}>
              {i > 0 && <div className={`split-divider${node.direction === 'v' ? ' split-divider-v' : ''}`} />}
              {renderPaneNode(child)}
            </Fragment>
          ))}
        </div>
      )
    }

    const leaf = node
    const isFocused = leaf.id === focusedPaneId
    const activeTab = leaf.tabs.find(t => t.id === leaf.activeTabId)
    const activePath = activeTab?.path ?? null

    return (
      <div className={`pane-col${isFocused ? ' pane-col-focused' : ''}`}>
        <TabBar
          paneId={leaf.id}
          tabs={leaf.tabs}
          activeTabId={leaf.activeTabId}
          onActivate={tabId => {
            setFocusedPaneId(leaf.id)
            setPaneRoot(prev => mapLeaf(prev, leaf.id, l => ({ ...l, activeTabId: tabId })))
            const tab = findLeaf(paneRootRef.current, leaf.id)?.tabs.find(t => t.id === tabId)
            if (tab) useEditorStore.getState().setCurrentPath(tab.path)
          }}
          onClose={tabId => closeTabInPane(leaf.id, tabId)}
          onTabMouseDown={(tabId, x, y) => {
            document.body.classList.add('dragging-tab')
            dragStateRef.current = { tabId, startX: x, startY: y, active: false, paneId: leaf.id }
          }}
          dragOverTabId={dragOverTabId}
          rightContent={
            <button
              className="icon-menubar-btn"
              title="關閉此面板"
              onClick={() => closePane(leaf.id)}
              style={{ fontSize: '16px' }}
            >×</button>
          }
        />
        <div
          ref={el => {
            if (el) paneElemsRef.current.set(leaf.id, el)
            else paneElemsRef.current.delete(leaf.id)
          }}
          className="editor-area"
          style={{ position: 'relative' }}
          onMouseDown={() => {
            if (leaf.id !== focusedPaneIdRef.current) {
              setFocusedPaneId(leaf.id)
              const currentLeaf = findLeaf(paneRootRef.current, leaf.id)
              const tab = currentLeaf?.tabs.find(t => t.id === currentLeaf.activeTabId)
              if (tab) useEditorStore.getState().setCurrentPath(tab.path)
            }
          }}
        >
          {renderPaneContent(leaf, isFocused, activePath)}
          {isDraggingTab && dropZoneInfo?.paneId === leaf.id && (
            <div className={`dz-indicator dz-${dropZoneInfo.zone}`} style={{ pointerEvents: 'none' }} />
          )}
        </div>
      </div>
    )
  }

  // ─── Early returns ─────────────────────────────────────────────────────
  if (!appReady) {
    return (
      <div className="app-loading">
        <div className="app-loading-spinner" />
        <span>載入中…</span>
      </div>
    )
  }
  if (showOnboarding) {
    return <Onboarding onComplete={handleOnboardingComplete} />
  }

  // ─── Main render ───────────────────────────────────────────────────────
  return (
    <div className="app-layout">
      <div className="content-row">

        {/* ── 左側 Icon Menubar ── */}
        <div className="icon-menubar">
          <button
            className={`icon-menubar-btn${leftPanel === 'files' ? ' active' : ''}`}
            title="檔案"
            onClick={() => setLeftPanel(p => p === 'files' ? null : 'files')}
          ><FontAwesomeIcon icon={faFolderTree} /></button>
          <button
            className={`icon-menubar-btn${leftPanel === 'search' ? ' active' : ''}`}
            title="搜尋"
            onClick={() => setLeftPanel(p => p === 'search' ? null : 'search')}
          ><FontAwesomeIcon icon={faMagnifyingGlass} /></button>
          <button
            className={`icon-menubar-btn${leftPanel === 'debug' ? ' active' : ''}`}
            title="Debug"
            onClick={() => setLeftPanel(p => p === 'debug' ? null : 'debug')}
          ><FontAwesomeIcon icon={faBug} /></button>
          <button
            className="icon-menubar-btn"
            title={leftPanel ? '收合側欄' : '展開側欄'}
            onClick={() => setLeftPanel(p => p ? null : 'files')}
          ><FontAwesomeIcon icon={leftPanel ? faChevronLeft : faChevronRight} /></button>

          <div className="icon-menubar-sep" />

          <button
            className={`icon-menubar-btn${currentPath === GRAPH_TAB ? ' active' : ''}`}
            title="圖譜"
            onClick={openGraphTab}
          ><FontAwesomeIcon icon={faSitemap} /></button>

          <div className="icon-menubar-sep" />

          <button
            className={`icon-menubar-btn${currentPath === AGENT_TOOLS_TAB ? ' active' : ''}`}
            title="Agent Tool 測試台"
            onClick={() => openNote(AGENT_TOOLS_TAB)}
          ><FontAwesomeIcon icon={faBolt} /></button>

          <div style={{ flex: 1 }} />

          <button
            className="icon-menubar-btn"
            title="設定 (⌘,)"
            onClick={() => setShowSettings(true)}
          ><FontAwesomeIcon icon={faGear} /></button>
        </div>

        {/* ── 左側面板 ── */}
        <aside
          className="sidebar"
          style={{ display: leftPanel ? undefined : 'none', width: sidebarWidth }}
        >
          <div style={{ display: leftPanel === 'files' ? 'flex' : 'none', flex: 1, flexDirection: 'column', overflow: 'hidden', minHeight: 0 }}>
            <div style={{ flex: 1, overflow: 'hidden', display: 'flex', flexDirection: 'column', minHeight: 0 }}>
              <FileTree onOpenNote={openNote} onOpenTrash={() => setShowTrash(true)} />
            </div>
          </div>
          <div style={{ display: leftPanel === 'search' ? 'flex' : 'none', flex: 1, flexDirection: 'column', height: '100%', overflow: 'hidden' }}>
            <SearchPanel onOpenNote={openNote} />
          </div>
          <div style={{ display: leftPanel === 'debug' ? 'flex' : 'none', flex: 1, flexDirection: 'column', height: '100%', overflow: 'hidden' }}>
            <DebugPanel />
          </div>
        </aside>

        {/* 左側分隔線 */}
        <div
          className="divider divider-left"
          style={{ display: leftPanel ? undefined : 'none', cursor: 'col-resize' }}
          onMouseDown={onLeftDividerMouseDown}
        />

        {/* ── Editor column — pane tree ── */}
        <div className="editor-column">
          {renderPaneNode(paneRoot)}
        </div>

        {/* 右側分隔線 */}
        {settings.enable_chat && (
          <div className="divider divider-right" onMouseDown={onRightDividerMouseDown} />
        )}

        {/* 右側欄 */}
        {settings.enable_chat && (
          <aside className="right-panel" style={{ width: rightPanelWidth }}>
            <div className="right-panel-tabs">
              <button className={`tab-btn ${rightTab === 'chat' ? 'active' : ''}`} onClick={() => setRightTab('chat')}>Chat</button>
              <button className={`tab-btn ${rightTab === 'live_chat' ? 'active' : ''}`} onClick={() => setRightTab('live_chat')}>Live Chat</button>
            </div>
            <div className="right-panel-content">
              <div style={{ display: rightTab === 'chat' ? 'contents' : 'none' }}>
                <ChatPanel liveChatActive={liveChatActive} />
              </div>
              <div style={{ display: rightTab === 'live_chat' ? 'contents' : 'none' }}>
                <LiveChatPanel onOpenNote={openNote} onActiveChange={setLiveChatActive} />
              </div>
            </div>
          </aside>
        )}

      </div>

      {/* Quick Open */}
      {showQuickOpen && (
        <QuickOpen
          onSelect={path => { openNote(path); setShowQuickOpen(false) }}
          onClose={() => setShowQuickOpen(false)}
        />
      )}

      {/* Settings */}
      {showSettings && <SettingsModal onClose={() => setShowSettings(false)} />}

      {/* Trash */}
      {showTrash && <TrashPanel onClose={() => setShowTrash(false)} />}

      {/* Drag ghost */}
      {isDraggingTab && dragPos && dragStateRef.current && (() => {
        const allTabs = getAllLeaves(paneRoot).flatMap(l => l.tabs)
        const tab = allTabs.find(t => t.id === dragStateRef.current!.tabId)
        return tab ? (
          <div style={{
            position: 'fixed', pointerEvents: 'none', zIndex: 9999,
            left: dragPos.x + 12, top: dragPos.y - 12,
            padding: '4px 10px', borderRadius: '5px', fontSize: '12px',
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-accent)',
            color: 'var(--color-text-primary)', boxShadow: '0 2px 8px rgba(0,0,0,0.25)',
            whiteSpace: 'nowrap', userSelect: 'none',
          }}>
            {getTabDisplayName(tab.path)}
          </div>
        ) : null
      })()}

      <Toast />
    </div>
  )
}
