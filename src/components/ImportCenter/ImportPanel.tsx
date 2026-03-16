import { useState, useEffect, useCallback, useRef } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import {
  faPlus, faTrash, faArrowsRotate, faDownload,
  faSpinner, faChevronLeft, faGlobe,
} from '@fortawesome/free-solid-svg-icons'
import { toast } from '../common/Toast'
import SiteOutlineTree from './SiteOutlineTree'
import DiffModal from './DiffModal'
import type { ImportPage } from './SiteOutlineTree'

interface ImportSession {
  session_id: string
  seed_url: string
  site_name: string
  root_folder: string
  status: string
  created_at: number
}

interface ImportSessionSummary extends ImportSession {
  total_pages: number
  imported_pages: number
}

interface PageUpdateInfo {
  page_id: string
  url: string
  title: string
  note_path: string
  new_content: string
}

interface Props {
  onOpenNote?: (path: string) => void
}

export default function ImportPanel({ onOpenNote }: Props) {
  const [sessions, setSessions] = useState<ImportSessionSummary[]>([])
  const [selectedSession, setSelectedSession] = useState<ImportSessionSummary | null>(null)
  const [pages, setPages] = useState<ImportPage[]>([])
  const [selectedPage, setSelectedPage] = useState<ImportPage | null>(null)
  const [importingIds, setImportingIds] = useState<Set<string>>(new Set())
  const [loadingSessions, setLoadingSessions] = useState(false)
  const [loadingPages, setLoadingPages] = useState(false)
  const [analyzingSession, setAnalyzingSession] = useState<string | null>(null)
  const [checkingUpdates, setCheckingUpdates] = useState(false)
  const [pendingUpdates, setPendingUpdates] = useState<PageUpdateInfo[]>([])
  const [showDiff, setShowDiff] = useState<PageUpdateInfo | null>(null)
  const [newUrl, setNewUrl] = useState('')
  const [showUrlInput, setShowUrlInput] = useState(false)
  const urlInputRef = useRef<HTMLInputElement>(null)

  const loadSessions = useCallback(async () => {
    setLoadingSessions(true)
    try {
      const result = await invoke<ImportSessionSummary[]>('list_import_sessions')
      setSessions(result)
    } catch (e: any) {
      toast.error(e?.message || String(e) || '載入匯入會話失敗')
    } finally {
      setLoadingSessions(false)
    }
  }, [])

  useEffect(() => { loadSessions() }, [loadSessions])

  useEffect(() => {
    if (showUrlInput) { setTimeout(() => urlInputRef.current?.focus(), 50) }
  }, [showUrlInput])

  const loadPages = useCallback(async (sessionId: string) => {
    setLoadingPages(true)
    setPages([])
    try {
      const result = await invoke<ImportPage[]>('get_session_pages', { sessionId })
      setPages(result)
    } catch (e: any) {
      toast.error(e?.message || String(e) || '載入頁面失敗')
    } finally {
      setLoadingPages(false)
    }
  }, [])

  const handleSelectSession = useCallback((session: ImportSessionSummary) => {
    setSelectedSession(session)
    setSelectedPage(null)
    loadPages(session.session_id)
  }, [loadPages])

  const handleCreateSession = useCallback(async () => {
    const url = newUrl.trim()
    if (!url) return
    try {
      await invoke<ImportSession>('create_import_session', { seedUrl: url })
      setNewUrl('')
      setShowUrlInput(false)
      await loadSessions()
      toast.success('已建立匯入會話')
    } catch (e: any) {
      toast.error(e?.message || String(e) || '建立會話失敗')
    }
  }, [newUrl, loadSessions])

  const handleDeleteSession = useCallback(async (sessionId: string) => {
    try {
      await invoke('delete_import_session', { sessionId })
      setSessions(s => s.filter(x => x.session_id !== sessionId))
      if (selectedSession?.session_id === sessionId) {
        setSelectedSession(null)
        setPages([])
      }
      toast.success('已刪除')
    } catch (e: any) {
      toast.error(e?.message || String(e) || '刪除失敗')
    }
  }, [selectedSession])

  const handleAnalyze = useCallback(async (sessionId: string) => {
    setAnalyzingSession(sessionId)
    try {
      const result = await invoke<ImportPage[]>('fetch_site_outline', { sessionId })
      setPages(result)
      await loadSessions()
      toast.success(`已分析到 ${result.length} 個頁面`)
    } catch (e: any) {
      toast.error(e?.message || String(e) || '分析失敗')
    } finally {
      setAnalyzingSession(null)
    }
  }, [loadSessions])

  const handleImportPage = useCallback(async (page: ImportPage) => {
    setImportingIds(s => new Set(s).add(page.page_id))
    try {
      const result = await invoke<{ note_path: string; title: string; was_updated: boolean }>(
        'import_page', { sessionId: page.session_id, pageId: page.page_id }
      )
      setPages(prev => prev.map(p =>
        p.page_id === page.page_id
          ? { ...p, status: 'imported', note_path: result.note_path }
          : p
      ))
      await loadSessions()
      toast.success(`已匯入：${result.title}`)
    } catch (e: any) {
      setPages(prev => prev.map(p =>
        p.page_id === page.page_id ? { ...p, status: 'failed' } : p
      ))
      toast.error(e?.message || String(e) || '匯入失敗')
    } finally {
      setImportingIds(s => { const n = new Set(s); n.delete(page.page_id); return n })
    }
  }, [loadSessions])

  const handleImportAll = useCallback(async () => {
    if (!selectedSession) return
    const pending = pages.filter(p => p.status === 'pending' || p.status === 'failed')
    if (pending.length === 0) { toast.error('沒有待匯入的頁面'); return }
    for (const page of pending) {
      await handleImportPage(page)
    }
  }, [selectedSession, pages, handleImportPage])

  const handleCheckUpdates = useCallback(async () => {
    if (!selectedSession) return
    setCheckingUpdates(true)
    try {
      const updates = await invoke<PageUpdateInfo[]>('check_page_updates', { sessionId: selectedSession.session_id })
      if (updates.length === 0) {
        toast.success('所有頁面都是最新的')
      } else {
        setPendingUpdates(updates)
        setShowDiff(updates[0])
        setPages(prev => prev.map(p => {
          const u = updates.find(u => u.page_id === p.page_id)
          return u ? { ...p, status: 'updated' } : p
        }))
      }
    } catch (e: any) {
      toast.error(e?.message || String(e) || '檢查更新失敗')
    } finally {
      setCheckingUpdates(false)
    }
  }, [selectedSession])

  const handleApplyUpdate = useCallback(async (pageId: string, _newContent: string) => {
    setShowDiff(null)
    setImportingIds(s => new Set(s).add(pageId))
    try {
      const result = await invoke<{ note_path: string; title: string; was_updated: boolean }>(
        'import_page', { sessionId: selectedSession!.session_id, pageId }
      )
      setPages(prev => prev.map(p =>
        p.page_id === pageId ? { ...p, status: 'imported', note_path: result.note_path } : p
      ))
      toast.success(`已更新：${result.title}`)
    } catch (e: any) {
      toast.error(e?.message || String(e) || '套用更新失敗')
    } finally {
      setImportingIds(s => { const n = new Set(s); n.delete(pageId); return n })
    }
    // Show next diff if any
    const remaining = pendingUpdates.filter(u => u.page_id !== pageId)
    setPendingUpdates(remaining)
    if (remaining.length > 0) setShowDiff(remaining[0])
  }, [selectedSession, pendingUpdates])

  const handleSkipUpdate = useCallback((pageId: string) => {
    setShowDiff(null)
    const remaining = pendingUpdates.filter(u => u.page_id !== pageId)
    setPendingUpdates(remaining)
    if (remaining.length > 0) setShowDiff(remaining[0])
  }, [pendingUpdates])

  const pendingCount = pages.filter(p => p.status === 'pending' || p.status === 'failed').length
  const importedCount = pages.filter(p => p.status === 'imported').length
  const updatedCount = pages.filter(p => p.status === 'updated').length

  // ── Render ──────────────────────────────────────────────────────────────────
  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', overflow: 'hidden' }}>
      {/* Header */}
      <div style={{
        padding: '10px 14px', borderBottom: '1px solid var(--color-border)',
        display: 'flex', alignItems: 'center', gap: 8, flexShrink: 0,
      }}>
        {selectedSession && (
          <button
            onClick={() => { setSelectedSession(null); setPages([]) }}
            style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--color-text-muted)', padding: '2px 4px' }}
          >
            <FontAwesomeIcon icon={faChevronLeft} />
          </button>
        )}
        <span style={{ fontWeight: 600, fontSize: 13, flex: 1, color: 'var(--color-text-primary)' }}>
          {selectedSession ? selectedSession.site_name || selectedSession.seed_url : '知識匯入'}
        </span>
        {!selectedSession && (
          <button
            title="新增匯入"
            onClick={() => setShowUrlInput(v => !v)}
            style={iconBtn}
          >
            <FontAwesomeIcon icon={faPlus} />
          </button>
        )}
        {selectedSession && (
          <>
            <button
              title="分析網站"
              disabled={!!analyzingSession}
              onClick={() => handleAnalyze(selectedSession.session_id)}
              style={iconBtn}
            >
              {analyzingSession === selectedSession.session_id
                ? <FontAwesomeIcon icon={faSpinner} spin />
                : <FontAwesomeIcon icon={faGlobe} />}
            </button>
            <button
              title="全部匯入"
              disabled={pendingCount === 0}
              onClick={handleImportAll}
              style={{ ...iconBtn, opacity: pendingCount === 0 ? 0.4 : 1 }}
            >
              <FontAwesomeIcon icon={faDownload} />
            </button>
            <button
              title="檢查更新"
              disabled={checkingUpdates || importedCount === 0}
              onClick={handleCheckUpdates}
              style={{ ...iconBtn, opacity: (checkingUpdates || importedCount === 0) ? 0.4 : 1 }}
            >
              <FontAwesomeIcon icon={checkingUpdates ? faSpinner : faArrowsRotate} spin={checkingUpdates} />
            </button>
          </>
        )}
      </div>

      {/* URL input */}
      {showUrlInput && !selectedSession && (
        <div style={{ padding: '8px 12px', borderBottom: '1px solid var(--color-border)', flexShrink: 0 }}>
          <div style={{ display: 'flex', gap: 6 }}>
            <input
              ref={urlInputRef}
              value={newUrl}
              onChange={e => setNewUrl(e.target.value)}
              onKeyDown={e => { if (e.key === 'Enter') handleCreateSession(); if (e.key === 'Escape') { setShowUrlInput(false); setNewUrl('') } }}
              placeholder="https://docs.example.com"
              style={{
                flex: 1, background: 'var(--color-bg)', border: '1px solid var(--color-border)',
                borderRadius: 6, padding: '5px 10px', fontSize: 12, color: 'var(--color-text-primary)',
                outline: 'none',
              }}
            />
            <button
              onClick={handleCreateSession}
              disabled={!newUrl.trim()}
              style={{
                padding: '5px 12px', background: 'var(--color-accent)', border: 'none',
                borderRadius: 6, color: '#fff', fontSize: 12, cursor: 'pointer',
                opacity: newUrl.trim() ? 1 : 0.5,
              }}
            >建立</button>
          </div>
        </div>
      )}

      {/* Main content */}
      {!selectedSession ? (
        /* Sessions list */
        <div style={{ flex: 1, overflowY: 'auto' }}>
          {loadingSessions ? (
            <div style={{ padding: 20, textAlign: 'center', color: 'var(--color-text-muted)', fontSize: 12 }}>
              <FontAwesomeIcon icon={faSpinner} spin /> 載入中…
            </div>
          ) : sessions.length === 0 ? (
            <div style={{ padding: '40px 20px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: 12 }}>
              <div style={{ marginBottom: 8 }}>尚無匯入會話</div>
              <div>點擊「+」輸入網站 URL 開始匯入</div>
            </div>
          ) : (
            sessions.map(session => (
              <div
                key={session.session_id}
                onClick={() => handleSelectSession(session)}
                style={{
                  padding: '10px 14px', cursor: 'pointer',
                  borderBottom: '1px solid var(--color-border)',
                  display: 'flex', alignItems: 'center', gap: 8,
                }}
                onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-hover)')}
                onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
              >
                <FontAwesomeIcon icon={faGlobe} style={{ color: 'var(--color-accent)', fontSize: 13, flexShrink: 0 }} />
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ fontSize: 13, fontWeight: 500, color: 'var(--color-text-primary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {session.site_name || session.seed_url}
                  </div>
                  <div style={{ fontSize: 11, color: 'var(--color-text-muted)', marginTop: 2, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {session.seed_url} · {session.imported_pages}/{session.total_pages} 頁已匯入
                  </div>
                </div>
                <button
                  title="刪除"
                  onClick={e => { e.stopPropagation(); handleDeleteSession(session.session_id) }}
                  style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--color-text-muted)', padding: '4px', opacity: 0.6 }}
                >
                  <FontAwesomeIcon icon={faTrash} style={{ fontSize: 11 }} />
                </button>
              </div>
            ))
          )}
        </div>
      ) : (
        /* Pages tree */
        <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
          {/* Stats bar */}
          <div style={{
            padding: '6px 14px', borderBottom: '1px solid var(--color-border)',
            fontSize: 11, color: 'var(--color-text-muted)', flexShrink: 0,
            display: 'flex', gap: 12,
          }}>
            <span>共 {pages.length} 頁</span>
            <span style={{ color: 'var(--color-success, #4caf50)' }}>✓ {importedCount} 已匯入</span>
            {pendingCount > 0 && <span>{pendingCount} 待匯入</span>}
            {updatedCount > 0 && <span style={{ color: 'var(--color-warning, #ff9800)' }}>↻ {updatedCount} 有更新</span>}
          </div>

          {loadingPages ? (
            <div style={{ padding: 20, textAlign: 'center', color: 'var(--color-text-muted)', fontSize: 12 }}>
              <FontAwesomeIcon icon={faSpinner} spin /> 載入頁面中…
            </div>
          ) : (
            <SiteOutlineTree
              pages={pages}
              importingIds={importingIds}
              selectedId={selectedPage?.page_id ?? null}
              onSelect={setSelectedPage}
              onImport={handleImportPage}
              onOpenNote={path => onOpenNote?.(path)}
            />
          )}

          {/* Selected page detail */}
          {selectedPage && (
            <div style={{
              borderTop: '1px solid var(--color-border)',
              padding: '10px 14px', flexShrink: 0,
              background: 'var(--color-bg)',
            }}>
              <div style={{ fontSize: 12, fontWeight: 500, color: 'var(--color-text-primary)', marginBottom: 4, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                {selectedPage.title || '（無標題）'}
              </div>
              <div style={{ fontSize: 11, color: 'var(--color-text-muted)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', marginBottom: 6 }}>
                {selectedPage.url}
              </div>
              <div style={{ display: 'flex', gap: 6 }}>
                {(selectedPage.status === 'pending' || selectedPage.status === 'failed') && (
                  <button
                    disabled={importingIds.has(selectedPage.page_id)}
                    onClick={() => handleImportPage(selectedPage)}
                    style={actionBtn}
                  >
                    {importingIds.has(selectedPage.page_id)
                      ? <><FontAwesomeIcon icon={faSpinner} spin /> 匯入中…</>
                      : '匯入此頁'}
                  </button>
                )}
                {selectedPage.note_path && (
                  <button
                    onClick={() => onOpenNote?.(selectedPage.note_path!)}
                    style={{ ...actionBtn, background: 'none', border: '1px solid var(--color-border)', color: 'var(--color-text-primary)' }}
                  >
                    開啟筆記
                  </button>
                )}
              </div>
            </div>
          )}
        </div>
      )}

      {/* Diff modal */}
      {showDiff && (
        <DiffModal
          update={showDiff}
          onApply={handleApplyUpdate}
          onSkip={handleSkipUpdate}
          onClose={() => setShowDiff(null)}
        />
      )}
    </div>
  )
}

const iconBtn: React.CSSProperties = {
  background: 'none', border: 'none', cursor: 'pointer',
  color: 'var(--color-text-muted)', padding: '4px 6px',
  borderRadius: 4, fontSize: 13,
}

const actionBtn: React.CSSProperties = {
  padding: '5px 12px', background: 'var(--color-accent)', border: 'none',
  borderRadius: 6, color: '#fff', fontSize: 12, cursor: 'pointer',
}
