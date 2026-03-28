import { useEffect, useState, useRef, useCallback, Fragment } from 'react'
import { useActivityStore, type ActivitySource } from './stores/activityStore'
import { useActivityContext } from './hooks/useActivityContext'
import { usePatternDetector } from './hooks/usePatternDetector'
import { listen } from '@tauri-apps/api/event'
import { invoke } from '@tauri-apps/api/core'
import { open as openPath } from '@tauri-apps/plugin-shell'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import { faGear, faBolt, faChevronLeft, faChevronRight, faSitemap, faFolderTree, faMagnifyingGlass, faBug, faComments, faMicrophone, faArrowRightArrowLeft, faTrash, faArrowRightFromBracket, faCircleQuestion, faUser, faFileImport, faShieldHalved, faSliders } from '@fortawesome/free-solid-svg-icons'
import { useSettingsStore } from './stores/settingsStore'
import { api } from './lib/api'
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
import SemanticSearchPanel from './components/Search/SemanticSearchPanel'
import DebugPanel from './components/Debug/DebugPanel'
import ChatPanel from './components/Chat/ChatPanel'
import FileViewer from './components/FileViewer/FileViewer'
import PreviewPanel from './components/Editor/PreviewPanel'
import LiveChatPanel from './components/LiveChat/LiveChatPanel'
import LiveChatSheet from './components/LiveChat/LiveChatSheet'
import NoteStatusBadge from './components/Editor/NoteStatusBadge'
import QuickOpen from './components/QuickOpen/QuickOpen'
import Spotlight, { type SpotlightFeature } from './components/Spotlight/Spotlight'
import SettingsModal from './components/Settings/SettingsModal'
import { AgentToolContent } from './components/AgentTools/AgentToolPanel'
import TrashPanel from './components/Trash/TrashPanel'
import ImportPanel from './components/ImportCenter/ImportPanel'
import SkillsPage from './components/Skills/SkillsPage'
import AgentsPage from './components/Agents/AgentsPage'
import KnowledgeAssistant from './components/KnowledgeAssistant/KnowledgeAssistant'
import HelpPanel from './components/Help/HelpPanel'
import Editor from './components/Editor/Editor'
import FileTree from './components/FileTree/FileTree'
import LoginScreen from './components/Auth/LoginScreen'
import VaultManagerModal from './components/Vault/VaultManagerModal'
import SetupWizard from './components/Setup/SetupWizard'
import TitleBar from './components/TitleBar/TitleBar'
import { useAuthStore } from './stores/authStore'
import Toast, { toast } from './components/common/Toast'
import ServerStatusBar from './components/common/ServerStatusBar'
import DaemonGuard from './components/common/DaemonGuard'
import { useTranslation } from 'react-i18next'
import './styles/App.css'

const GRAPH_TAB = '__graph__'
const AGENT_TOOLS_TAB = '__agent_tools__'
const CHAT_TAB = '__chat__'
const LIVE_CHAT_TAB = '__live_chat__'
const SETTINGS_TAB = '__settings__'
const SYSTEM_SETTINGS_TAB = '__system_settings__'
const HELP_TAB = '__help__'
const TRASH_TAB = '__trash__'
const IMPORT_TAB = '__import__'
const KB_ASSIST_TAB = '__kb_assist__'
const SKILLS_TAB = '__skills__'
const AGENTS_TAB = '__agents__'
const DEBUG_TAB = '__debug__'

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
  const { session, isLoading: authLoading, checkSession } = useAuthStore()
  const [appReady, setAppReady] = useState(false)

  // Windows platform detection — adds data-platform="windows" to body for CSS targeting
  useEffect(() => {
    if (navigator.userAgent.includes('Windows')) {
      document.body.dataset.platform = 'windows'
    }
  }, [])

  // F12 → open DevTools (production debug helper)
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'F12') invoke('open_devtools').catch(() => {})
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [])

  // 首次掛載驗證 session
  useEffect(() => { checkSession() }, [])

  // DB/init 就緒偵測：先 poll is_app_ready（後端可能比前端先啟動完），再監聽 app:ready 事件
  useEffect(() => {
    let cancelled = false
    let unlisten: (() => void) | null = null
    invoke<boolean>('is_app_ready').then(ready => {
      if (ready && !cancelled) setAppReady(true)
    }).catch(() => {})
    listen('app:ready', () => {
      if (!cancelled) setAppReady(true)
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [])

  const { t } = useTranslation()

  // Auth + app loading splash
  if (authLoading || !appReady) {
    return (
      <DaemonGuard>
        <div style={{ display: 'flex', flexDirection: 'column', width: '100%', height: '100vh', background: 'var(--color-bg-base)' }}>
          <TitleBar />
          <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', color: 'var(--color-text-muted)', fontSize: '13px' }}>
            {t('app.loading')}
          </div>
        </div>
      </DaemonGuard>
    )
  }

  // Not authenticated — show login
  if (!session) return (
    <DaemonGuard>
      <div style={{ display: 'flex', flexDirection: 'column', width: '100%', height: '100vh' }}>
        <TitleBar />
        <LoginScreen />
      </div>
    </DaemonGuard>
  )

  return (
    <DaemonGuard>
      <AppMain />
    </DaemonGuard>
  )
}

function AppMain() {
  const { t } = useTranslation()
  const { session, logout: authLogout } = useAuthStore()
  const { load: loadSettings, settings, savePersonal: saveSettings, saveSystem } = useSettingsStore()
  const { scanVault, setupWatchers, readNote, loadNotes } = useVaultStore()
  const { load: loadGraph } = useGraphStore()
  const { currentPath, pendingAnchor } = useEditorStore()
  const { push: navPush, back: navBack, forward: navForward, canGoBack, canGoForward } = useNavigationStore()
  useActivityContext()   // 全域 selectionchange / copy 監聽
  usePatternDetector()  // 行為模式偵測 + Bayesian 評分

  const [appReady, setAppReady] = useState(false)
  const [showSetupWizard, setShowSetupWizard] = useState(false)
  const [showVaultManager, setShowVaultManager] = useState(false)
  const [showQuickOpen, setShowQuickOpen] = useState(false)
  const [showSpotlight, setShowSpotlight] = useState(false)
  const [liveChatSheetOpen, setLiveChatSheetOpen] = useState(false)
  const [showVcredistWarning, setShowVcredistWarning] = useState(false)
  const [userMenuOpen, setUserMenuOpen] = useState(false)
  const userMenuRef = useRef<HTMLDivElement>(null)
  const [sidebarWidth, setSidebarWidth] = useState(240)
  const [leftPanel, setLeftPanel] = useState<'files' | 'search' | null>('files')

  // Windows: check VC++ Redist and show warning if missing
  useEffect(() => {
    if (navigator.userAgent.includes('Windows')) {
      invoke<boolean>('check_vcredist').then(installed => {
        if (!installed) setShowVcredistWarning(true)
      }).catch(() => {})
    }
  }, [])

  // Tracks whether each chat/livechat tab (by tab id) is currently "active"
  // (streaming or recording) — used for close confirmation
  const chatActiveRef = useRef<Map<string, boolean>>(new Map())
  // Paths being renamed — prevents vault:note-deleted from closing those tabs
  const pendingRenamesRef = useRef<Set<string>>(new Set())

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

  // ─── Sync all open pane paths to activityStore ────────────────────────
  useEffect(() => {
    const paths = getAllLeaves(paneRoot)
      .map(l => l.tabs.find(t => t.id === l.activeTabId)?.path)
      .filter((p): p is string => typeof p === 'string' && !p.startsWith('__'))
    useActivityStore.getState().setOpenPaths(paths)
  }, [paneRoot])

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
  const editorContent = useEditorStore(s => s.content)
  useEffect(() => {
    const editorPath = useEditorStore.getState().currentPath
    if (!editorPath) return
    getAllLeaves(paneRoot).filter(l => l.id !== focusedPaneId).forEach(leaf => {
      const tab = leaf.tabs.find(t => t.id === leaf.activeTabId)
      if (tab?.path === editorPath) {
        setPaneContents(prev => prev[leaf.id] === editorContent ? prev : { ...prev, [leaf.id]: editorContent })
      }
    })
  }, [paneRoot, focusedPaneId, editorContent])

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
      setSidebarWidth(settings.sidebar_width)
      const needsSetup = !settings.whisper_cli_path || !settings.llama_cli_path ||
        !settings.whisper_model_path || !settings.llm_model_path
      if (needsSetup) {
        setShowSetupWizard(true)
      } else if (settings.personal_current_vault_path) {
        await handleVaultSelect(settings.personal_current_vault_path)
      } else {
        setShowVaultManager(true)
      }
    }
    init()
  }, [])

  // ─── Vault select (from VaultManagerModal) ─────────────────────────────
  const handleVaultSelect = useCallback(async (newVaultPath: string) => {
    const { settings } = useSettingsStore.getState()
    const recentVaults = [
      newVaultPath,
      ...(settings.recent_vaults ?? []).filter(v => v !== newVaultPath),
    ].slice(0, 10)
    await saveSystem({ system_current_vault_path: newVaultPath } as any)
    await saveSettings({ onboarding_done: true, recent_vaults: recentVaults, personal_current_vault_path: newVaultPath })
    useEditorStore.getState().setCurrentPath(null)
    // Reset pane tree to single empty leaf
    const newLeafId = crypto.randomUUID()
    initialLeafIdRef.current = newLeafId
    setPaneRoot({ kind: 'leaf', id: newLeafId, tabs: [], activeTabId: null })
    setFocusedPaneId(newLeafId)
    // Fast path: load notes from existing daemon index + graph in parallel
    const [lastNote] = await Promise.all([
      api.getVaultLastNote(newVaultPath).catch(() => null),
      loadNotes(),
      loadGraph(),
    ])
    if (lastNote) {
      const tabId = crypto.randomUUID()
      setPaneRoot({ kind: 'leaf', id: newLeafId, tabs: [{ id: tabId, path: lastNote }], activeTabId: tabId })
      useEditorStore.getState().setCurrentPath(lastNote)
      navPush(lastNote)
    }
    setShowVaultManager(false)
    setAppReady(true)
    // Background: re-index any files added outside the app (non-blocking)
    scanVault().catch(() => {})
  }, [saveSettings, scanVault, loadNotes, loadGraph, navPush])

  // ─── Save last open note ───────────────────────────────────────────────
  useEffect(() => {
    if (!currentPath || !settings.system_current_vault_path || currentPath === GRAPH_TAB) return
    api.setVaultLastNote(settings.system_current_vault_path, currentPath).catch(() => {})
  }, [currentPath, settings.system_current_vault_path])

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
          loadingToastId = toast.info(t('app.whisper_loading'), { duration: 0 })
      } else if (line.includes('就緒')) {
        if (loadingToastId !== null) { toast.dismiss(loadingToastId); loadingToastId = null }
        toast.info(t('app.whisper_ready'))
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
          loadingToastId = toast.info(t('app.llm_loading'), { duration: 0 })
      } else if (line.includes('就緒')) {
        if (loadingToastId !== null) { toast.dismiss(loadingToastId); loadingToastId = null }
        toast.info(t('app.llm_ready'))
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [])

  // ─── FileWatcher + graph sync ──────────────────────────────────────────
  useEffect(() => {
    if (!appReady) return
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
        console.log('[ui:open_note] received:', e.payload)
        if (e.payload) openNoteFromChat(e.payload)
      })
      const uiToastUnlisten = await listen<{ message: string; kind: string; duration_ms: number }>('ui:toast', e => {
        const { message, kind } = e.payload
        if (kind === 'error') toast.error(message)
        else if (kind === 'success') toast.success(message)
        else toast.info(message)
      })
      const uiActionUnlisten = await listen<{ action: string; payload?: unknown }>('ui:action', e => {
        const { action, payload } = e.payload
        if (action === 'open_chat') openNote(CHAT_TAB)
        else if (action === 'open_live_chat') openNote(LIVE_CHAT_TAB)
        else if (action === 'open_settings') openNote(SETTINGS_TAB)
        else if (action === 'open_note' && typeof payload === 'string') openNoteFromChat(payload)
        else console.warn('[ui:action] unknown action:', action, payload)
      })
      const dbCorruptionUnlisten = await listen<{ reason: string }>('db:corruption_detected', e => {
        toast.error(
          `⚠️ 資料庫索引損壞（${e.payload.reason}）\n請前往「語意搜尋 → 修復 DB」完成修復後重啟 App。`,
          { duration: 0 }
        )
      })
      const scheduleTriggeredUnlisten = await listen<{ task_id: string; description: string }>('schedule:triggered', e => {
        toast.info(`排程提醒：${e.payload.description}`)
      })
      const noteDeletedUnlisten = await listen<string[]>('vault:note-deleted', e => {
        const vaultPath = useSettingsStore.getState().settings.system_current_vault_path
        const deletedRelPaths = e.payload
          .filter(abs => abs.startsWith(vaultPath + '/'))
          .map(abs => abs.slice(vaultPath.length + 1))
        deletedRelPaths.forEach(relPath => {
          if (pendingRenamesRef.current.has(relPath)) return
          getAllLeaves(paneRootRef.current).forEach(leaf => {
            leaf.tabs.filter(t => t.path === relPath).forEach(t => {
              closeTabInPane(leaf.id, t.id)
            })
          })
          toast.error(`「${relPath.split('/').pop()}」已被刪除`)
        })
      })
      const handleNoteRenamed = (e: Event) => {
        const { oldPath, newPath } = (e as CustomEvent).detail as { oldPath: string; newPath: string }
        pendingRenamesRef.current.add(oldPath)
        setTimeout(() => pendingRenamesRef.current.delete(oldPath), 3000)
        getAllLeaves(paneRootRef.current).forEach(leaf => {
          const updated = leaf.tabs.map(t => t.path === oldPath ? { ...t, path: newPath } : t)
          if (updated.some((t, i) => t.path !== leaf.tabs[i].path)) {
            setPaneRoot(prev => mapLeaf(prev, leaf.id, l => ({ ...l, tabs: updated })))
            if (leaf.tabs.find(t => t.id === leaf.activeTabId)?.path === oldPath) {
              useEditorStore.getState().setCurrentPath(newPath)
            }
          }
        })
      }
      window.addEventListener('note:renamed', handleNoteRenamed)
      cleanup = () => {
        vaultCleanup()
        graphListeners.forEach(u => u())
        vaultChangedUnlisten()
        openNoteUnlisten()
        uiToastUnlisten()
        uiActionUnlisten()
        dbCorruptionUnlisten()
        scheduleTriggeredUnlisten()
        noteDeletedUnlisten()
        window.removeEventListener('note:renamed', handleNoteRenamed)
      }
    }
    setup()
    return () => cleanup?.()
  }, [appReady])

  // ─── Keyboard shortcuts ────────────────────────────────────────────────
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const meta = e.metaKey || e.ctrlKey
      if (meta && e.key === 'p' && !e.shiftKey) { e.preventDefault(); setShowQuickOpen(true) }
      if (meta && e.key === 'k' && !e.shiftKey) { e.preventDefault(); setShowSpotlight(true) }
      if (meta && e.key === ',') { e.preventDefault(); openNote(SETTINGS_TAB) }
      if (meta && e.altKey && e.key.toLowerCase() === 'i') { e.preventDefault(); setLiveChatSheetOpen(v => !v) }
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
  // Chat/LiveChat tabs should never be replaced by content navigation
  const isChatTab = (p: string) => p === CHAT_TAB || p === LIVE_CHAT_TAB || p === IMPORT_TAB || p === KB_ASSIST_TAB || p === SKILLS_TAB || p === AGENTS_TAB

  // Find a pane that is NOT showing a Chat/LiveChat tab as its active tab,
  // excluding the given leafId. Returns undefined if none exists.
  const findNonChatPane = useCallback((root: PaneNode, excludeId: string) => {
    return getAllLeaves(root).find(l => {
      if (l.id === excludeId) return false
      const at = l.tabs.find(t => t.id === l.activeTabId)
      return !at || !isChatTab(at.path)
    })
  }, [])

  const openNote = useCallback((path: string, opts?: { source?: ActivitySource; fromPath?: string }) => {
    // Activity tracking：只記錄真實筆記（非特殊 tab）
    if (!path.startsWith('__')) {
      const isWikilink = opts?.source === 'wikilink'
      useActivityStore.getState().addAction({
        type: isWikilink ? 'wikilink_click' : 'note_open',
        path,
        fromPath: opts?.fromPath,
        source: opts?.source ?? 'tab',
      })
    }

    const root = paneRootRef.current
    const leafId = focusedPaneIdRef.current
    const leaf = findLeaf(root, leafId)
    if (!leaf) return

    // If focused pane is showing Chat/LiveChat, protect it — open in another pane
    const activeTab = leaf.tabs.find(t => t.id === leaf.activeTabId)
    if (activeTab && isChatTab(activeTab.path)) {
      const other = findNonChatPane(root, leafId)
      if (other) {
        const existing = other.tabs.find(t => t.path === path)
        if (existing) {
          setPaneRoot(prev => mapLeaf(prev, other.id, l => ({ ...l, activeTabId: existing.id })))
        } else {
          const id = crypto.randomUUID()
          setPaneRoot(prev => mapLeaf(prev, other.id, l => ({
            ...l, tabs: [...l.tabs, { id, path }], activeTabId: id,
          })))
        }
        setFocusedPaneId(other.id)
      } else {
        // No non-chat pane — split to create a new pane for the note
        const newLeafId = crypto.randomUUID()
        const tabId = crypto.randomUUID()
        const newLeaf: PaneLeaf = { kind: 'leaf', id: newLeafId, tabs: [{ id: tabId, path }], activeTabId: tabId }
        setPaneRoot(prev => splitLeaf(prev, leafId, 'h', newLeaf))
        setFocusedPaneId(newLeafId)
      }
      useEditorStore.getState().setCurrentPath(path)
      navPush(path)
      return
    }

    const existing = leaf.tabs.find(t => t.path === path)
    if (existing) {
      setPaneRoot(prev => mapLeaf(prev, leafId, l => ({ ...l, activeTabId: existing.id })))
    } else if (leaf.activeTabId) {
      // Replace current tab's path (browser-like navigation)
      setPaneRoot(prev => mapLeaf(prev, leafId, l => ({
        ...l, tabs: l.tabs.map(t => t.id === l.activeTabId ? { ...t, path } : t),
      })))
    } else {
      const id = crypto.randomUUID()
      setPaneRoot(prev => mapLeaf(prev, leafId, l => ({
        ...l, tabs: [...l.tabs, { id, path }], activeTabId: id,
      })))
    }
    useEditorStore.getState().setCurrentPath(path)
    navPush(path)
  }, [navPush, findNonChatPane])

  // ─── Open a file in a new tab (keep existing tabs intact) ──────────────
  const openNoteInNewTab = useCallback((path: string) => {
    const root = paneRootRef.current
    const leafId = focusedPaneIdRef.current
    const leaf = findLeaf(root, leafId)
    if (!leaf) return

    // If focused pane is showing Chat/LiveChat, protect it — open in another pane
    const activeTab = leaf.tabs.find(t => t.id === leaf.activeTabId)
    if (activeTab && isChatTab(activeTab.path)) {
      const other = findNonChatPane(root, leafId)
      if (other) {
        const existing = other.tabs.find(t => t.path === path)
        if (existing) {
          setPaneRoot(prev => mapLeaf(prev, other.id, l => ({ ...l, activeTabId: existing.id })))
        } else {
          const id = crypto.randomUUID()
          setPaneRoot(prev => mapLeaf(prev, other.id, l => ({
            ...l, tabs: [...l.tabs, { id, path }], activeTabId: id,
          })))
        }
        setFocusedPaneId(other.id)
      } else {
        // No non-chat pane — split to create a new pane for the note
        const newLeafId = crypto.randomUUID()
        const tabId = crypto.randomUUID()
        const newLeaf: PaneLeaf = { kind: 'leaf', id: newLeafId, tabs: [{ id: tabId, path }], activeTabId: tabId }
        setPaneRoot(prev => splitLeaf(prev, leafId, 'h', newLeaf))
        setFocusedPaneId(newLeafId)
      }
      useEditorStore.getState().setCurrentPath(path)
      navPush(path)
      return
    }

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
  }, [navPush, findNonChatPane])

  // ─── Open a note from Chat/LiveChat in a non-focused pane ──────────────
  // Keeps the focused editor intact so the user can continue chatting.
  // If there's already a second pane, opens there; otherwise splits the
  // focused pane horizontally to create one.
  const openNoteFromChat = useCallback((pathWithAnchor: string) => {
    // Support "path#section" encoding from chunk search results
    const hashIdx = pathWithAnchor.indexOf('#')
    const rawPath = hashIdx >= 0 ? pathWithAnchor.slice(0, hashIdx) : pathWithAnchor
    const anchor  = hashIdx >= 0 ? pathWithAnchor.slice(hashIdx + 1) : undefined

    // Convert absolute path → relative (DB stores relative; agent:note_refs emits absolute)
    const vaultRoot = useSettingsStore.getState().settings.system_current_vault_path
    const normalizedVault = vaultRoot.replace(/\\/g, '/').replace(/\/$/, '')
    const normalizedRaw   = rawPath.replace(/\\/g, '/')
    const path = normalizedRaw.startsWith(normalizedVault + '/')
      ? normalizedRaw.slice(normalizedVault.length + 1)
      : rawPath

    const root = paneRootRef.current
    const focusedId = focusedPaneIdRef.current
    // Use findNonChatPane so we never overwrite a Chat/LiveChat/Import pane's content
    const otherLeaf = findNonChatPane(root, focusedId)

    if (otherLeaf) {
      // Re-use existing non-chat, non-focused pane
      const existing = otherLeaf.tabs.find(t => t.path === path)
      if (existing) {
        setPaneRoot(prev => mapLeaf(prev, otherLeaf.id, l => ({ ...l, activeTabId: existing.id })))
      } else {
        const id = crypto.randomUUID()
        setPaneRoot(prev => mapLeaf(prev, otherLeaf.id, l => ({
          ...l, tabs: [...l.tabs, { id, path }], activeTabId: id,
        })))
      }
    } else {
      // No non-chat pane — split to create a new pane for the note
      const newLeafId = crypto.randomUUID()
      const tabId = crypto.randomUUID()
      const newLeaf: PaneLeaf = { kind: 'leaf', id: newLeafId, tabs: [{ id: tabId, path }], activeTabId: tabId }
      setPaneRoot(prev => splitLeaf(prev, focusedId, 'h', newLeaf))
      // focusedPaneId intentionally unchanged — user stays in the chat/import pane
    }

    // Set anchor AFTER pane update so Editor picks it up when content loads
    useEditorStore.getState().setPendingAnchor(anchor)
  }, [findNonChatPane])

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

    // Confirm if closing an active chat / live-chat tab
    const closingTab = leaf.tabs.find(t => t.id === tabId)
    if (closingTab && (closingTab.path === CHAT_TAB || closingTab.path === LIVE_CHAT_TAB)) {
      if (chatActiveRef.current.get(tabId)) {
        if (!window.confirm(t('app.chat_active_confirm'))) return
      }
      chatActiveRef.current.delete(tabId)
    }

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

  // ─── Close a pane entirely (discard its tabs) ─────────────────────────
  const closePane = useCallback((paneId: string) => {
    const root = paneRootRef.current
    const leaves = getAllLeaves(root)
    if (leaves.length <= 1) {
      // Only one pane — clear all tabs instead of removing the pane
      setPaneRoot(prev => mapLeaf(prev, paneId, l => ({ ...l, tabs: [], activeTabId: null })))
      useEditorStore.getState().setCurrentPath(null)
      return
    }
    const targetLeaf = leaves.find(l => l.id !== paneId)
    if (!targetLeaf) return
    const newRoot = removeLeaf(root, paneId)
    if (!newRoot) return
    setPaneRoot(newRoot)
    if (focusedPaneIdRef.current === paneId) {
      setFocusedPaneId(targetLeaf.id)
      const tl = findLeaf(newRoot, targetLeaf.id)
      const tab = tl?.tabs.find(t => t.id === tl.activeTabId)
      if (tab) useEditorStore.getState().setCurrentPath(tab.path)
    }
  }, [])

  const openGraphTab = useCallback(() => openNote(GRAPH_TAB), [openNote])

  // ─── Spotlight features list ───────────────────────────────────────────
  const spotlightFeatures: SpotlightFeature[] = [
    { id: 'graph', label: '知識圖譜', icon: '🕸️', action: () => openNote(GRAPH_TAB) },
    { id: 'agents', label: 'Agent 管理', icon: '⚡', action: () => openNote(AGENTS_TAB) },
    { id: 'skills', label: '技能規範', icon: '🎚️', action: () => openNote(SKILLS_TAB) },
    { id: 'kb_assist', label: '知識助理', icon: '🛡️', action: () => openNote(KB_ASSIST_TAB) },
    { id: 'import', label: '匯入中心', icon: '📥', action: () => openNote(IMPORT_TAB) },
    { id: 'chat', label: 'Chat 對話', icon: '💬', action: () => openNote(CHAT_TAB) },
    { id: 'settings', label: '個人設定', icon: '👤', action: () => openNote(SETTINGS_TAB) },
    { id: 'system_settings', label: '系統設定', icon: '⚙️', action: () => openNote(SYSTEM_SETTINGS_TAB) },
    { id: 'trash', label: '垃圾桶', icon: '🗑️', action: () => openNote(TRASH_TAB) },
    { id: 'help', label: '說明', icon: '❓', action: () => openNote(HELP_TAB) },
    { id: 'search', label: '語義搜尋', icon: '🔎', action: () => setLeftPanel('search') },
  ]

  // ─── Sidebar resize ────────────────────────────────────────────────────
  const onLeftDividerMouseDown = () => { isDraggingLeft.current = true }
  useEffect(() => {
    const onMouseMove = (e: MouseEvent) => {
      if (isDraggingLeft.current) setSidebarWidth(Math.max(180, Math.min(400, e.clientX - 44)))
    }
    const onMouseUp = () => { isDraggingLeft.current = false }
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
    if (activePath === SETTINGS_TAB) {
      const settingsTab = leaf.tabs.find(t => t.path === SETTINGS_TAB)
      const closeSettings = settingsTab ? () => closeTabInPane(leaf.id, settingsTab.id) : undefined
      return (
        <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
          <SettingsModal inline mode="personal" onClose={closeSettings} />
        </div>
      )
    }
    if (activePath === SYSTEM_SETTINGS_TAB) {
      const settingsTab = leaf.tabs.find(t => t.path === SYSTEM_SETTINGS_TAB)
      const closeSettings = settingsTab ? () => closeTabInPane(leaf.id, settingsTab.id) : undefined
      return (
        <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
          <SettingsModal inline mode="system" onClose={closeSettings} />
        </div>
      )
    }
    if (activePath === HELP_TAB) return (
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
        <HelpPanel />
      </div>
    )
    if (activePath === TRASH_TAB) return (
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
        <TrashPanel inline />
      </div>
    )
    if (activePath === DEBUG_TAB) return (
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
        <DebugPanel />
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
        pendingAnchor={pendingAnchor}
        onAnchorScrolled={() => useEditorStore.getState().setPendingAnchor(undefined)}
        onWikilinkClick={async title => {
          if (/\.[^/.]+$/.test(title)) {
            try {
              const assets = await invoke<string[]>('list_assets')
              const lower = title.toLowerCase()
              const match = assets.find(a => a.toLowerCase() === lower || a.toLowerCase().endsWith('/' + lower))
              if (match) {
                const vaultPath = useSettingsStore.getState().settings.system_current_vault_path.replace(/\\/g, '/')
                await openPath(`${vaultPath}/${match}`)
              }
            } catch { /* ignore */ }
            return
          }
          const note = useVaultStore.getState().notes.find(n => n.title === title)
          if (note) openNote(note.path, { source: 'wikilink', fromPath: activePath })
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
            leaf.tabs.length > 0 ? (
              <div style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
                {isFocused && <NoteStatusBadge />}
                <button
                  className="icon-menubar-btn"
                  title="關閉此面板"
                  onClick={() => closePane(leaf.id)}
                  style={{ fontSize: '16px', width: '19px', flexShrink: 0 }}
                >×</button>
              </div>
            ) : undefined
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
          {/* Always-mounted Chat / LiveChat / Import / KB Assistant / Skills panels — hidden with display:none when not active */}
          {leaf.tabs.filter(t => t.path === CHAT_TAB || t.path === LIVE_CHAT_TAB || t.path === IMPORT_TAB || t.path === KB_ASSIST_TAB || t.path === SKILLS_TAB || t.path === AGENTS_TAB).map(t => (
            <div key={t.id} style={{
              display: t.id === leaf.activeTabId ? 'flex' : 'none',
              flex: 1, flexDirection: 'column', height: '100%', overflow: 'hidden',
            }}>
              {t.path === CHAT_TAB && (
                <ChatPanel
                  liveChatActive={false}
                  onActiveChange={active => { chatActiveRef.current.set(t.id, active) }}
                  onOpenNote={openNoteFromChat}
                />
              )}
              {t.path === LIVE_CHAT_TAB && (
                <LiveChatPanel
                  onOpenNote={openNoteFromChat}
                  onActiveChange={active => { chatActiveRef.current.set(t.id, active) }}
                />
              )}
              {t.path === IMPORT_TAB && (
                <ImportPanel />
              )}
              {t.path === SKILLS_TAB && (
                <SkillsPage />
              )}
              {t.path === AGENTS_TAB && (
                <AgentsPage />
              )}
              {t.path === KB_ASSIST_TAB && (
                <KnowledgeAssistant onOpenNote={openNoteFromChat} />
              )}
            </div>
          ))}
          {/* Regular content — only when active tab is not Chat/LiveChat/Import */}
          {(!activeTab || (activeTab.path !== CHAT_TAB && activeTab.path !== LIVE_CHAT_TAB && activeTab.path !== IMPORT_TAB && activeTab.path !== KB_ASSIST_TAB && activeTab.path !== SKILLS_TAB && activeTab.path !== AGENTS_TAB)) && (
            renderPaneContent(leaf, isFocused, activePath)
          )}
          {isDraggingTab && dropZoneInfo?.paneId === leaf.id && (
            <div className={`dz-indicator dz-${dropZoneInfo.zone}`} style={{ pointerEvents: 'none' }} />
          )}
        </div>
      </div>
    )
  }

  // ─── Early returns ─────────────────────────────────────────────────────
  if (!appReady && !showVaultManager && !showSetupWizard) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', width: '100%', height: '100vh', background: 'var(--color-bg-base)' }}>
        <TitleBar />
        <div className="app-loading">
          <div className="app-loading-spinner" />
          <span>{t('app.loading')}</span>
        </div>
      </div>
    )
  }
  if (showSetupWizard && !appReady) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', width: '100%', height: '100vh', background: 'var(--color-bg-base)' }}>
        <TitleBar />
        <SetupWizard onDone={() => { setShowSetupWizard(false); setShowVaultManager(true) }} />
      </div>
    )
  }
  if (showVaultManager && !appReady) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', width: '100%', height: '100vh', background: 'var(--color-bg-base)' }}>
        <TitleBar />
        <VaultManagerModal onSelect={handleVaultSelect} canClose={false} />
      </div>
    )
  }

  // ─── Main render ───────────────────────────────────────────────────────
  const vaultName = settings.system_current_vault_path ? settings.system_current_vault_path.split('/').pop() : undefined

  return (
    <div className="app-layout">
      <TitleBar title={vaultName} />
      {showVcredistWarning && (
        <div style={{ background: '#7c3a00', color: '#ffd7a0', fontSize: '12px', padding: '6px 12px', display: 'flex', alignItems: 'center', gap: 8, flexShrink: 0 }}>
          <span style={{ flex: 1 }}>未偵測到 Visual C++ 2015-2022 Redistributable (x64)，llama-server 可能無法啟動。</span>
          <a href="https://aka.ms/vs/17/release/vc_redist.x64.exe" target="_blank" rel="noreferrer" style={{ color: '#ffd7a0', textDecoration: 'underline', flexShrink: 0 }}>下載安裝</a>
          <button onClick={() => setShowVcredistWarning(false)} style={{ background: 'none', border: 'none', color: '#ffd7a0', cursor: 'pointer', padding: '0 4px', fontSize: '14px', flexShrink: 0 }}>✕</button>
        </div>
      )}
      <div className="content-row">

        {/* ── 左側 Icon Menubar ── */}
        <div className="icon-menubar">
          {/* 收合 / 展開 — 最上方 */}
          <button
            className="icon-menubar-btn"
            title={leftPanel ? t('sidebar.collapse') : t('sidebar.expand')}
            onClick={() => setLeftPanel(p => p ? null : 'files')}
          ><FontAwesomeIcon icon={leftPanel ? faChevronLeft : faChevronRight} /></button>

          <div className="icon-menubar-sep" />

          {/* 側欄面板切換 */}
          <button
            className={`icon-menubar-btn${leftPanel === 'files' ? ' active' : ''}`}
            title={t('sidebar.files')}
            onClick={() => setLeftPanel(p => p === 'files' ? null : 'files')}
          ><FontAwesomeIcon icon={faFolderTree} /></button>
          <button
            className={`icon-menubar-btn${leftPanel === 'search' ? ' active' : ''}`}
            title={t('sidebar.search')}
            onClick={() => setLeftPanel(p => p === 'search' ? null : 'search')}
          ><FontAwesomeIcon icon={faMagnifyingGlass} /></button>
          {(settings.show_spotlight ?? true) && (
            <button
              className="icon-menubar-btn"
              title="Spotlight 搜尋 (⌘K)"
              onClick={() => setShowSpotlight(true)}
            >⌘</button>
          )}
          {settings.debug_mode && (
            <button
              className={`icon-menubar-btn${currentPath === DEBUG_TAB ? ' active' : ''}`}
              title="Debug"
              onClick={() => openNote(DEBUG_TAB)}
            ><FontAwesomeIcon icon={faBug} /></button>
          )}

          <div className="icon-menubar-sep" />

          {/* 特殊頁籤：圖譜 / Agent / Chat / Live Chat */}
          {settings.show_graph && (
            <button
              className={`icon-menubar-btn${currentPath === GRAPH_TAB ? ' active' : ''}`}
              title={t('tabs.graph')}
              onClick={openGraphTab}
            ><FontAwesomeIcon icon={faSitemap} /></button>
          )}

          {settings.show_agents && (
            <button
              className={`icon-menubar-btn${currentPath === AGENTS_TAB ? ' active' : ''}`}
              title={t('tabs.agents')}
              onClick={() => openNote(AGENTS_TAB)}
            ><FontAwesomeIcon icon={faBolt} /></button>
          )}

          {settings.show_skills && (
            <button
              className={`icon-menubar-btn${currentPath === SKILLS_TAB ? ' active' : ''}`}
              title={t('tabs.skills')}
              onClick={() => openNote(SKILLS_TAB)}
            ><FontAwesomeIcon icon={faSliders} /></button>
          )}

          {settings.show_kb_assist && (
            <button
              className={`icon-menubar-btn${currentPath === KB_ASSIST_TAB ? ' active' : ''}`}
              title={t('tabs.kb_assist')}
              onClick={() => openNote(KB_ASSIST_TAB)}
            ><FontAwesomeIcon icon={faShieldHalved} /></button>
          )}

          {settings.show_import && (
            <button
              className={`icon-menubar-btn${currentPath === IMPORT_TAB ? ' active' : ''}`}
              title={t('tabs.import')}
              onClick={() => openNote(IMPORT_TAB)}
            ><FontAwesomeIcon icon={faFileImport} /></button>
          )}

          {settings.show_agent_tools && (
            <button
              className={`icon-menubar-btn${currentPath === AGENT_TOOLS_TAB ? ' active' : ''}`}
              title={t('tabs.agent_tools')}
              onClick={() => openNote(AGENT_TOOLS_TAB)}
            ><FontAwesomeIcon icon={faBolt} /></button>
          )}

          {settings.enable_chat && <>
            <button
              className="icon-menubar-btn"
              title={t('tabs.chat')}
              onClick={() => openNote(CHAT_TAB)}
            ><FontAwesomeIcon icon={faComments} /></button>
            <button
              className={`icon-menubar-btn${liveChatSheetOpen ? ' active' : ''}`}
              title={`${t('tabs.live_chat')} (⌘⌥I)`}
              onClick={() => setLiveChatSheetOpen(v => !v)}
            ><FontAwesomeIcon icon={faMicrophone} /></button>
          </>}

          <div style={{ flex: 1 }} />

          {/* User profile button */}
          <div ref={userMenuRef} style={{ position: 'relative' }}>
            <button
              className="icon-menubar-btn"
              title={session?.username ?? '帳號'}
              onClick={() => setUserMenuOpen(v => !v)}
              style={{
                width: '32px', height: '32px', borderRadius: '50%', overflow: 'hidden',
                background: settings.avatar_type === 'image' && settings.avatar_image
                  ? 'transparent'
                  : (settings.avatar_color || 'var(--color-accent)'),
                color: '#fff',
                fontSize: '13px', fontWeight: 700,
                display: 'flex', alignItems: 'center', justifyContent: 'center',
                border: 'none', padding: 0, cursor: 'pointer', flexShrink: 0,
              }}
            >
              {settings.avatar_type === 'image' && settings.avatar_image
                ? <img src={settings.avatar_image} style={{ width: '100%', height: '100%', objectFit: 'cover' }} />
                : (settings.display_name?.slice(0, 2).toUpperCase() || session?.username?.charAt(0).toUpperCase() || '?')}
            </button>

            {userMenuOpen && (
              <div style={{
                position: 'absolute', bottom: '40px', left: '0',
                background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
                borderRadius: '8px', boxShadow: '0 4px 20px rgba(0,0,0,0.35)',
                zIndex: 300, minWidth: '180px', padding: '4px 0', overflow: 'hidden',
              }}>
                {/* User info header */}
                <div style={{ padding: '10px 14px 8px', borderBottom: '1px solid var(--color-border)' }}>
                  <div style={{ fontSize: '13px', fontWeight: 600, color: 'var(--color-text-primary)' }}>
                    {settings.display_name || session?.username}
                  </div>
                  {settings.display_name && (
                    <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '2px' }}>
                      @{session?.username}
                    </div>
                  )}
                </div>

                {/* Menu items */}
                {([
                  { icon: faUser, label: t('settings.title'), action: () => { openNote(SETTINGS_TAB); setUserMenuOpen(false) } },
                  { icon: faGear, label: t('settings.system_title'), action: () => { openNote(SYSTEM_SETTINGS_TAB); setUserMenuOpen(false) } },
                  { icon: faCircleQuestion, label: t('help.title'), action: () => { openNote(HELP_TAB); setUserMenuOpen(false) } },
                ] as const).map(item => (
                  <button key={item.label} onClick={item.action} style={{
                    display: 'flex', alignItems: 'center', gap: '10px',
                    width: '100%', padding: '8px 14px', fontSize: '13px',
                    color: 'var(--color-text-secondary)', background: 'transparent',
                    cursor: 'pointer', textAlign: 'left',
                  }}
                    onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
                    onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
                  >
                    <FontAwesomeIcon icon={item.icon} style={{ width: '14px', flexShrink: 0 }} />
                    {item.label}
                  </button>
                ))}

                <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />

                {([
                  { icon: faArrowRightArrowLeft, label: t('vault.select_title'), action: () => { setShowVaultManager(true); setUserMenuOpen(false) } },
                  { icon: faTrash, label: t('trash.title'), action: () => { openNote(TRASH_TAB); setUserMenuOpen(false) } },
                ] as const).map(item => (
                  <button key={item.label} onClick={item.action} style={{
                    display: 'flex', alignItems: 'center', gap: '10px',
                    width: '100%', padding: '8px 14px', fontSize: '13px',
                    color: 'var(--color-text-secondary)', background: 'transparent',
                    cursor: 'pointer', textAlign: 'left',
                  }}
                    onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
                    onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
                  >
                    <FontAwesomeIcon icon={item.icon} style={{ width: '14px', flexShrink: 0 }} />
                    {item.label}
                  </button>
                ))}

                <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0' }} />

                <button onClick={() => { authLogout(); setUserMenuOpen(false) }} style={{
                  display: 'flex', alignItems: 'center', gap: '10px',
                  width: '100%', padding: '8px 14px', fontSize: '13px',
                  color: 'var(--color-danger, #e06c75)', background: 'transparent',
                  cursor: 'pointer', textAlign: 'left',
                }}
                  onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
                  onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
                >
                  <FontAwesomeIcon icon={faArrowRightFromBracket} style={{ width: '14px', flexShrink: 0 }} />
                  {t('auth.logout')}
                </button>
              </div>
            )}
          </div>
        </div>

        {/* ── 左側面板 ── */}
        <aside
          className="sidebar"
          style={{ width: leftPanel ? sidebarWidth : 0, minWidth: leftPanel ? 180 : 0, borderRight: leftPanel ? undefined : 'none' }}
        >
          <div style={{ display: leftPanel === 'files' ? 'flex' : 'none', flex: 1, flexDirection: 'column', overflow: 'hidden', minHeight: 0 }}>
            <div style={{ flex: 1, overflow: 'hidden', display: 'flex', flexDirection: 'column', minHeight: 0 }}>
              <FileTree
                onOpenNote={path => openNote(path, { source: 'filetree' })}
                onOpenNoteInNewTab={openNoteInNewTab}
              />
            </div>
          </div>
          <div style={{ display: leftPanel === 'search' ? 'flex' : 'none', flex: 1, flexDirection: 'column', height: '100%', overflow: 'hidden' }}>
            <SemanticSearchPanel onOpenNote={openNoteFromChat} />
          </div>
          <ServerStatusBar onOpenSettings={() => openNote(SYSTEM_SETTINGS_TAB)} />
        </aside>

        {/* 左側分隔線 */}
        <div
          className="divider divider-left"
          style={{ opacity: leftPanel ? 1 : 0, pointerEvents: leftPanel ? undefined : 'none', transition: 'opacity 0.18s ease', cursor: 'col-resize' }}
          onMouseDown={onLeftDividerMouseDown}
        />

        {/* ── Editor column — pane tree ── */}
        {/* Always wrap root in pane-area so root element type never changes across
            leaf→group transitions. Fragment key={leaf.id} ensures React reconciles
            (not unmounts) the leaf pane when the root pane is split, preserving
            mounted special panels (Chat / LiveChat / Import) and their state. */}
        <div className="editor-column">
          {paneRoot.kind === 'leaf' ? (
            <div className="pane-area">
              <Fragment key={paneRoot.id}>
                {renderPaneNode(paneRoot)}
              </Fragment>
            </div>
          ) : renderPaneNode(paneRoot)}
        </div>


      </div>

      {/* Quick Open */}
      {showQuickOpen && (
        <QuickOpen
          onSelect={path => { openNote(path, { source: 'quickopen' }); setShowQuickOpen(false) }}
          onClose={() => setShowQuickOpen(false)}
        />
      )}

      {/* Spotlight */}
      {showSpotlight && (
        <Spotlight
          features={spotlightFeatures}
          onSelectNote={path => { openNote(path, { source: 'quickopen' }); setShowSpotlight(false) }}
          onClose={() => setShowSpotlight(false)}
        />
      )}

      {/* Vault Manager (switch vault overlay) */}
      {showVaultManager && appReady && (
        <VaultManagerModal
          onSelect={handleVaultSelect}
          canClose={true}
          onClose={() => setShowVaultManager(false)}
        />
      )}

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

      {/* Live Chat Sheet — global overlay, app-level lifecycle */}
      {liveChatSheetOpen && settings.enable_chat && (
        <LiveChatSheet
          open={liveChatSheetOpen}
          onClose={() => setLiveChatSheetOpen(false)}
          onOpenNote={path => { openNoteFromChat(path); setLiveChatSheetOpen(false) }}
          onOpenTab={tab => {
            const tabMap: Record<string, string> = {
              settings: SETTINGS_TAB, trash: TRASH_TAB, agents: AGENTS_TAB, skills: SKILLS_TAB,
            }
            if (tabMap[tab]) openNote(tabMap[tab])
          }}
          onShowResults={paths => {
            if (paths.length === 1) { openNoteFromChat(paths[0]); setLiveChatSheetOpen(false) }
          }}
        />
      )}

      <Toast />
    </div>
  )
}
