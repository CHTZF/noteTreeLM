import { useEffect, useState, useRef, useCallback } from 'react'
import { listen } from '@tauri-apps/api/event'
import { useSettingsStore } from './stores/settingsStore'
import { useVaultStore } from './stores/vaultStore'
import { useGraphStore } from './stores/graphStore'
import { useEditorStore } from './stores/editorStore'
import { useNavigationStore } from './stores/navigationStore'
import Onboarding from './components/Onboarding/Onboarding'
import FileTree from './components/FileTree/FileTree'
import Editor from './components/Editor/Editor'
import GraphView from './components/Graph/GraphView'
import SearchPanel from './components/Search/SearchPanel'
import DebugPanel from './components/Debug/DebugPanel'
import ChatPanel from './components/Chat/ChatPanel'
import QuickOpen from './components/QuickOpen/QuickOpen'
import SettingsModal from './components/Settings/SettingsModal'
import TrashPanel from './components/Trash/TrashPanel'
import Toast from './components/common/Toast'
import './styles/App.css'

type RightPanelTab = 'graph' | 'search' | 'debug' | 'chat'

export default function App() {
  const { load: loadSettings, settings } = useSettingsStore()
  const { scanVault, setupWatchers } = useVaultStore()
  const { load: loadGraph } = useGraphStore()
  const { setCurrentPath } = useEditorStore()
  const { push: navPush, back: navBack, forward: navForward, canGoBack, canGoForward } = useNavigationStore()

  const [appReady, setAppReady] = useState(false)
  const [showOnboarding, setShowOnboarding] = useState(false)
  const [showSettings, setShowSettings] = useState(false)
  const [showTrash, setShowTrash] = useState(false)
  const [rightTab, setRightTab] = useState<RightPanelTab>('graph')
  const [showQuickOpen, setShowQuickOpen] = useState(false)
  const [sidebarWidth, setSidebarWidth] = useState(240)
  const [rightPanelWidth, setRightPanelWidth] = useState(320)
  const isDraggingLeft = useRef(false)
  const isDraggingRight = useRef(false)

  // 套用佈景主題 + 字型到 DOM
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

  // 初始化
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
      }
      setAppReady(true)
    }
    init()
  }, [])

  // Debug mode 關閉時，若當前在 debug tab 則切回 graph
  useEffect(() => {
    if (!settings.debug_mode && rightTab === 'debug') setRightTab('graph')
  }, [settings.debug_mode])

  // Chat 關閉時，若當前在 chat tab 則切回 graph
  useEffect(() => {
    if (!settings.enable_chat && rightTab === 'chat') setRightTab('graph')
  }, [settings.enable_chat])

  // FileWatcher + 圖譜同步
  useEffect(() => {
    if (!appReady || showOnboarding) return
    let cleanup: (() => void) | undefined

    const setup = async () => {
      const vaultCleanup = await setupWatchers()
      // 新增/刪除/重命名筆記時也重新載入圖譜
      const graphListeners = await Promise.all(
        ['vault:note-created', 'vault:note-deleted', 'vault:note-renamed'].map(
          (event) => listen(event, () => loadGraph())
        )
      )
      cleanup = () => {
        vaultCleanup()
        graphListeners.forEach((u) => u())
      }
    }

    setup()
    return () => cleanup?.()
  }, [appReady, showOnboarding])

  // 鍵盤快捷鍵
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const meta = e.metaKey || e.ctrlKey
      if (meta && e.key === 'p' && !e.shiftKey) {
        e.preventDefault()
        setShowQuickOpen(true)
      }
      if (meta && e.key === ',') {
        e.preventDefault()
        setShowSettings(true)
      }
      if (meta && e.key === '[') {
        e.preventDefault()
        const prev = navBack()
        if (prev) setCurrentPath(prev)
      }
      if (meta && e.key === ']') {
        e.preventDefault()
        const next = navForward()
        if (next) setCurrentPath(next)
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [navBack, navForward])

  const openNote = useCallback((path: string) => {
    navPush(path)
    setCurrentPath(path)
  }, [navPush, setCurrentPath])

  const handleOnboardingComplete = useCallback(async () => {
    setShowOnboarding(false)
    const { settings } = useSettingsStore.getState()
    setSidebarWidth(settings.sidebar_width)
    setRightPanelWidth(settings.graph_panel_width)
    await scanVault()
    await loadGraph()
  }, [])

  // 左側分隔線拖曳
  const onLeftDividerMouseDown = () => { isDraggingLeft.current = true }
  const onRightDividerMouseDown = () => { isDraggingRight.current = true }

  useEffect(() => {
    const onMouseMove = (e: MouseEvent) => {
      if (isDraggingLeft.current) {
        setSidebarWidth(Math.max(180, Math.min(400, e.clientX)))
      }
      if (isDraggingRight.current) {
        const w = window.innerWidth - e.clientX
        setRightPanelWidth(Math.max(240, Math.min(480, w)))
      }
    }
    const onMouseUp = () => {
      isDraggingLeft.current = false
      isDraggingRight.current = false
    }
    window.addEventListener('mousemove', onMouseMove)
    window.addEventListener('mouseup', onMouseUp)
    return () => {
      window.removeEventListener('mousemove', onMouseMove)
      window.removeEventListener('mouseup', onMouseUp)
    }
  }, [])

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

  return (
    <div className="app-layout">
      {/* 左側欄：FileTree + Settings 按鈕 */}
      <aside className="sidebar" style={{ width: sidebarWidth }}>
        <div style={{ flex: 1, overflow: 'hidden', display: 'flex', flexDirection: 'column', minHeight: 0 }}>
          <FileTree onOpenNote={openNote} />
        </div>
        <div style={{ padding: '8px 12px', borderTop: '1px solid var(--color-border)', flexShrink: 0, display: 'flex', alignItems: 'center', gap: '4px' }}>
          <button
            onClick={() => setShowSettings(true)}
            title="設定 (⌘,)"
            style={{ display: 'flex', alignItems: 'center', gap: '6px', color: 'var(--color-text-secondary)', fontSize: '13px', flex: 1, padding: '4px 0' }}
            onMouseEnter={(e) => (e.currentTarget.style.color = 'var(--color-text-primary)')}
            onMouseLeave={(e) => (e.currentTarget.style.color = 'var(--color-text-secondary)')}
          >
            <span style={{ fontSize: '15px' }}>⚙</span>
            <span>設定</span>
          </button>
          <button
            onClick={() => setShowTrash(true)}
            title="垃圾桶"
            style={{ color: 'var(--color-text-secondary)', fontSize: '16px', padding: '4px 6px', borderRadius: '5px', flexShrink: 0, opacity: 0.6 }}
            onMouseEnter={(e) => { (e.currentTarget as HTMLElement).style.opacity = '1'; (e.currentTarget as HTMLElement).style.background = 'var(--color-bg-hover)' }}
            onMouseLeave={(e) => { (e.currentTarget as HTMLElement).style.opacity = '0.6'; (e.currentTarget as HTMLElement).style.background = 'transparent' }}
          >🗑</button>
        </div>
      </aside>

      {/* 左側拖曳分隔線 */}
      <div className="divider divider-left" onMouseDown={onLeftDividerMouseDown} />

      {/* 中間：編輯器 */}
      <main className="editor-area">
        <Editor
          canGoBack={canGoBack}
          canGoForward={canGoForward}
          onBack={() => { const p = navBack(); if (p) setCurrentPath(p) }}
          onForward={() => { const p = navForward(); if (p) setCurrentPath(p) }}
        />
      </main>

      {/* 右側拖曳分隔線 */}
      <div className="divider divider-right" onMouseDown={onRightDividerMouseDown} />

      {/* 右側欄：Graph + Search */}
      <aside className="right-panel" style={{ width: rightPanelWidth }}>
        <div className="right-panel-tabs">
          <button
            className={`tab-btn ${rightTab === 'graph' ? 'active' : ''}`}
            onClick={() => setRightTab('graph')}
          >
            圖譜
          </button>
          <button
            className={`tab-btn ${rightTab === 'search' ? 'active' : ''}`}
            onClick={() => setRightTab('search')}
          >
            搜索
          </button>
          {settings.enable_chat && (
            <button
              className={`tab-btn ${rightTab === 'chat' ? 'active' : ''}`}
              onClick={() => setRightTab('chat')}
            >
              Chat
            </button>
          )}
          {settings.debug_mode && (
            <button
              className={`tab-btn ${rightTab === 'debug' ? 'active' : ''}`}
              onClick={() => setRightTab('debug')}
              style={{ color: rightTab === 'debug' ? 'var(--color-warning, #f59e0b)' : undefined }}
            >
              Debug
            </button>
          )}
        </div>
        <div className="right-panel-content">
          <div style={{ display: rightTab === 'graph' ? 'contents' : 'none' }}>
            <GraphView onOpenNote={openNote} />
          </div>
          <div style={{ display: rightTab === 'search' ? 'contents' : 'none' }}>
            <SearchPanel onOpenNote={openNote} />
          </div>
          {settings.enable_chat && (
            <div style={{ display: rightTab === 'chat' ? 'contents' : 'none' }}>
              <ChatPanel />
            </div>
          )}
          {settings.debug_mode && (
            <div style={{ display: rightTab === 'debug' ? 'contents' : 'none' }}>
              <DebugPanel />
            </div>
          )}
        </div>
      </aside>

      {/* Quick Open */}
      {showQuickOpen && (
        <QuickOpen
          onSelect={(path) => { openNote(path); setShowQuickOpen(false) }}
          onClose={() => setShowQuickOpen(false)}
        />
      )}

      {/* Settings Modal */}
      {showSettings && <SettingsModal onClose={() => setShowSettings(false)} />}

      {/* Trash Panel */}
      {showTrash && <TrashPanel onClose={() => setShowTrash(false)} />}

      {/* Toast 通知容器 */}
      <Toast />
    </div>
  )
}
