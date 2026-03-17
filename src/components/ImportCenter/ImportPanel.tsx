import { useState, useEffect, useCallback, useRef } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import {
  faPlus, faTrash, faArrowsRotate, faDownload,
  faSpinner, faGlobe, faChevronLeft, faChevronRight,
  faFileLines, faPaperPlane, faDatabase, faCheckCircle, faListCheck,
} from '@fortawesome/free-solid-svg-icons'
import { toast } from '../common/Toast'
import { useDebugStore } from '../../stores/debugStore'
import { useAuthStore } from '../../stores/authStore'
import { useKnowledgeChatStore, type KBMessage, type KnowledgeRef } from '../../stores/knowledgeChatStore'

// ── Types ─────────────────────────────────────────────────────────────────────

interface ImportSession {
  session_id: string
  seed_url: string
  site_name: string
  root_folder: string
  status: string
  created_at: number
  auto_update?: boolean
}

interface ImportSessionSummary extends ImportSession {
  total_pages: number
  imported_pages: number
}

interface KBCardSuggestion {
  title: string
  template: 'concept' | 'procedure' | 'reference'
  content: string
  reason: string
}

interface Props {
  onOpenNote?: (path: string) => void
}

function fmtError(e: unknown): string {
  if (typeof e === 'string') return e
  if (e && typeof e === 'object') {
    const obj = e as Record<string, unknown>
    if (typeof obj.message === 'string') return obj.message
    return JSON.stringify(e)
  }
  return String(e)
}

// ── Main Component ─────────────────────────────────────────────────────────────

export default function ImportPanel({ onOpenNote }: Props) {
  const addLog = useDebugStore(s => s.addLog)
  const { session } = useAuthStore()

  // ── Sessions state ──────────────────────────────────────────────────────────
  const [sessions, setSessions] = useState<ImportSessionSummary[]>([])
  const [loadingSessions, setLoadingSessions] = useState(false)
  const [showSidebar, setShowSidebar] = useState(true)
  const [showAddSource, setShowAddSource] = useState(false)
  const [newUrl, setNewUrl] = useState('')
  const [addingSource, setAddingSource] = useState(false)
  const urlInputRef = useRef<HTMLInputElement>(null)

  // ── Q&A state ───────────────────────────────────────────────────────────────
  const messages = useKnowledgeChatStore(s => s.messages)
  const setMessages = useKnowledgeChatStore(s => s.setMessages)
  const selectedSessionId = useKnowledgeChatStore(s => s.selectedSessionId)
  const setSelectedSessionIdStore = useKnowledgeChatStore(s => s.setSelectedSessionId)
  const [conversationId, setConversationId] = useState<string | null>(null)
  const [input, setInput] = useState('')
  const [isQuerying, setIsQuerying] = useState(false)
  const pendingRefsRef = useRef<KnowledgeRef[]>([])
  const messagesEndRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)

  // ── Source management overlay state ─────────────────────────────────────────
  const [showManage, setShowManage] = useState(false)
  const [managingSession, setManagingSession] = useState<ImportSessionSummary | null>(null)

  // ── Load sessions ────────────────────────────────────────────────────────────
  const loadSessions = useCallback(async () => {
    setLoadingSessions(true)
    try {
      const result = await invoke<ImportSessionSummary[]>('list_import_sessions')
      setSessions(result)
    } catch (e: unknown) {
      const msg = fmtError(e)
      addLog('import', 'error', `載入會話失敗：${msg}`)
    } finally {
      setLoadingSessions(false)
    }
  }, [addLog])

  useEffect(() => { loadSessions() }, [loadSessions])

  // 監聽自動更新通知
  useEffect(() => {
    const unlisten = listen<{ session_id: string; count: number }>('import:updates_available', e => {
      const { session_id, count } = e.payload
      const s = sessions.find(s => s.session_id === session_id)
      const name = s?.site_name || session_id
      toast.success(`${name} 有 ${count} 頁已更新，可重新匯入`)
    })
    return () => { unlisten.then(f => f()) }
  }, [sessions])

  // ── Conversation persistence ─────────────────────────────────────────────────
  const saveMessagesToDb = useCallback(async (convId: string, msgs: KBMessage[]) => {
    const payload = msgs
      .filter(m => !m.isStreaming && !m.error)
      .map(m => ({ id: m.id, role: m.role, content: m.content, refs: m.refs }))
    try {
      await invoke('save_conversation_messages', {
        conversationId: convId,
        messagesJson: JSON.stringify(payload),
      })
    } catch { /* non-critical */ }
  }, [])

  // 初始化：從 DB 恢復上次 knowledge_qa 對話
  useEffect(() => {
    const username = session?.username ?? ''
    console.log('[ImportPanel] mount, username:', username)
    if (!username) return
    invoke<string | null>('get_last_mode_conversation_id', { username, mode: 'knowledge_qa' })
      .then(saved => {
        console.log('[ImportPanel] last knowledge_qa conversation_id:', saved)
        if (!saved) return
        return invoke<{ messages_json: string }>('get_conversation', { id: saved })
          .then(snap => {
            setConversationId(saved)
            const msgs: Array<{ id?: string; role: string; content: string; refs?: KnowledgeRef[] }>
              = JSON.parse(snap.messages_json)
            const restored = msgs
              .filter(m => m.role === 'user' || m.role === 'assistant')
              .map(m => ({
                id: m.id ?? crypto.randomUUID(),
                role: m.role as 'user' | 'assistant',
                content: m.content,
                refs: m.refs,
              }))
            console.log('[ImportPanel] restored', restored.length, 'messages for conversation', saved)
            setMessages(() => restored)
          })
          .catch((e) => {
            console.warn('[ImportPanel] get_conversation failed, clearing stale ID:', e)
            invoke('set_last_mode_conversation_id', { username, mode: 'knowledge_qa', conversationId: null }).catch(() => {})
          })
      })
      .catch(() => {})
  }, [session?.username, setMessages])

  useEffect(() => {
    if (showAddSource) setTimeout(() => urlInputRef.current?.focus(), 50)
  }, [showAddSource])

  // ── Auto scroll ──────────────────────────────────────────────────────────────
  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  // ── Add source ───────────────────────────────────────────────────────────────
  const handleAddSource = useCallback(async () => {
    const url = newUrl.trim()
    if (!url || addingSource) return
    setAddingSource(true)
    try {
      await invoke<ImportSession>('create_import_session', { seedUrl: url })
      setNewUrl('')
      setShowAddSource(false)
      await loadSessions()
      toast.success('已新增來源')
    } catch (e: unknown) {
      const msg = fmtError(e)
      addLog('import', 'error', `新增來源失敗：${msg}`)
      toast.error(msg || '新增來源失敗')
    } finally {
      setAddingSource(false)
    }
  }, [newUrl, addingSource, loadSessions, addLog])

  const handleDeleteSession = useCallback(async (sessionId: string) => {
    try {
      await invoke('delete_import_session', { sessionId })
      if (selectedSessionId === sessionId) setSelectedSessionIdStore(null)
      if (managingSession?.session_id === sessionId) {
        setManagingSession(null)
        setShowManage(false)
      }
      setSessions(s => s.filter(x => x.session_id !== sessionId))
      toast.success('已刪除來源')
    } catch (e: unknown) {
      const msg = fmtError(e)
      addLog('import', 'error', `刪除失敗：${msg}`)
      toast.error(msg || '刪除失敗')
    }
  }, [selectedSessionId, managingSession, addLog])

  // ── Q&A streaming ────────────────────────────────────────────────────────────
  const handleQuery = useCallback(async () => {
    const q = input.trim()
    if (!q || isQuerying) return

    // Ensure we have a conversation (create one on first query)
    const username = session?.username ?? ''
    let convId = conversationId
    if (!convId && username) {
      try {
        convId = await invoke<string>('create_conversation', { username, mode: 'knowledge_qa' })
        setConversationId(convId)
        invoke('set_last_mode_conversation_id', { username, mode: 'knowledge_qa', conversationId: convId }).catch(() => {})
      } catch { /* proceed without persistence */ }
    }

    const queryId = crypto.randomUUID()
    const userMsgId = crypto.randomUUID()
    const assistantMsgId = crypto.randomUUID()
    setMessages(prev => [
      ...prev,
      { id: userMsgId, role: 'user', content: q },
      { id: assistantMsgId, role: 'assistant', content: '', isStreaming: true },
    ])
    setInput('')
    setIsQuerying(true)
    pendingRefsRef.current = []

    // Set up listeners BEFORE invoke to avoid race condition
    const unToken = await listen<{ query_id: string; content: string }>('knowledge:token', e => {
      if (e.payload.query_id !== queryId) return
      setMessages(prev => {
        const last = prev[prev.length - 1]
        if (!last || last.role !== 'assistant') return prev
        return [...prev.slice(0, -1), { ...last, content: last.content + e.payload.content }]
      })
    })

    const unRefs = await listen<{ query_id: string; refs: KnowledgeRef[] }>('knowledge:refs', e => {
      if (e.payload.query_id !== queryId) return
      pendingRefsRef.current = e.payload.refs
    })

    let unDone: (() => void) | null = null
    unDone = await listen<{ query_id: string; error?: string }>('knowledge:done', e => {
      if (e.payload.query_id !== queryId) return
      const currentRefs = pendingRefsRef.current
      pendingRefsRef.current = []
      setMessages(prev => {
        const updated = prev.map(m =>
          m.id === assistantMsgId
            ? { ...m, isStreaming: false, error: e.payload.error, refs: currentRefs }
            : m
        )
        // Save to DB after updating (convId captured in closure)
        if (convId) saveMessagesToDb(convId, updated)
        return updated
      })
      setIsQuerying(false)
      unToken(); unRefs(); unDone?.()
    })

    try {
      await invoke('query_knowledge', {
        queryId,
        question: q,
        sessionId: selectedSessionId ?? undefined,
      })
    } catch (e: unknown) {
      const msg = fmtError(e)
      addLog('import', 'error', `knowledge query 失敗：${msg}`)
      setMessages(prev => prev.map(m =>
        m.id === assistantMsgId ? { ...m, isStreaming: false, error: msg } : m
      ))
      setIsQuerying(false)
      unToken(); unRefs(); unDone?.()
    }
  }, [input, isQuerying, selectedSessionId, conversationId, session?.username, addLog, saveMessagesToDb])

  const selectedSession = sessions.find(s => s.session_id === selectedSessionId) ?? null

  // ── Render ────────────────────────────────────────────────────────────────────
  return (
    <div style={{ display: 'flex', height: '100%', overflow: 'hidden', background: 'var(--color-bg-base)' }}>

      {/* ── Left Sidebar: Sources ── */}
      {showSidebar && (
        <div style={{
          width: 220, flexShrink: 0, borderRight: '1px solid var(--color-border)',
          display: 'flex', flexDirection: 'column', overflow: 'hidden',
          background: 'var(--color-bg)',
        }}>
          {/* Sidebar header */}
          <div style={{
            padding: '10px 12px 8px', borderBottom: '1px solid var(--color-border)',
            display: 'flex', alignItems: 'center', gap: 6, flexShrink: 0,
          }}>
            <FontAwesomeIcon icon={faDatabase} style={{ fontSize: 11, color: 'var(--color-accent)' }} />
            <span style={{ fontSize: 12, fontWeight: 600, flex: 1, color: 'var(--color-text-primary)' }}>知識來源</span>
            <button
              title="新增來源"
              onClick={() => setShowAddSource(v => !v)}
              style={iconBtn}
            >
              <FontAwesomeIcon icon={faPlus} />
            </button>
          </div>

          {/* Add source input */}
          {showAddSource && (
            <div style={{ padding: '8px 10px', borderBottom: '1px solid var(--color-border)', flexShrink: 0 }}>
              <input
                ref={urlInputRef}
                value={newUrl}
                onChange={e => setNewUrl(e.target.value)}
                onKeyDown={e => {
                  if (e.key === 'Enter') handleAddSource()
                  if (e.key === 'Escape') { setShowAddSource(false); setNewUrl('') }
                }}
                placeholder="https://docs.example.com"
                style={{
                  width: '100%', boxSizing: 'border-box',
                  background: 'var(--color-bg)', border: '1px solid var(--color-border)',
                  borderRadius: 5, padding: '4px 8px', fontSize: 11,
                  color: 'var(--color-text-primary)', outline: 'none', marginBottom: 5,
                }}
              />
              <button
                onClick={handleAddSource}
                disabled={!newUrl.trim() || addingSource}
                style={{
                  width: '100%', padding: '4px', background: 'var(--color-accent)',
                  border: 'none', borderRadius: 5, color: '#fff', fontSize: 11,
                  cursor: 'pointer', opacity: (!newUrl.trim() || addingSource) ? 0.5 : 1,
                }}
              >
                {addingSource ? '新增中…' : '新增'}
              </button>
            </div>
          )}

          {/* Sessions list */}
          <div style={{ flex: 1, overflowY: 'auto' }}>
            {/* All sources option */}
            <div
              onClick={() => setSelectedSessionIdStore(null)}
              style={{
                padding: '7px 12px', cursor: 'pointer', fontSize: 12,
                background: selectedSessionId === null ? 'var(--color-bg-hover)' : 'transparent',
                color: selectedSessionId === null ? 'var(--color-accent)' : 'var(--color-text-secondary)',
                fontWeight: selectedSessionId === null ? 600 : 400,
                borderBottom: '1px solid var(--color-border)',
                display: 'flex', alignItems: 'center', gap: 6,
              }}
              onMouseEnter={e => { if (selectedSessionId !== null) e.currentTarget.style.background = 'var(--color-bg-hover)' }}
              onMouseLeave={e => { if (selectedSessionId !== null) e.currentTarget.style.background = 'transparent' }}
            >
              <FontAwesomeIcon icon={faDatabase} style={{ fontSize: 10, flexShrink: 0 }} />
              所有來源
            </div>

            {loadingSessions ? (
              <div style={{ padding: '12px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: 11 }}>
                <FontAwesomeIcon icon={faSpinner} spin />
              </div>
            ) : sessions.length === 0 ? (
              <div style={{ padding: '16px 12px', color: 'var(--color-text-muted)', fontSize: 11, textAlign: 'center', lineHeight: 1.5 }}>
                尚無來源<br />點 + 新增網站
              </div>
            ) : sessions.map(session => (
              <div
                key={session.session_id}
                onClick={() => setSelectedSessionIdStore(session.session_id)}
                style={{
                  padding: '7px 10px 7px 12px', cursor: 'pointer',
                  background: selectedSessionId === session.session_id ? 'var(--color-bg-hover)' : 'transparent',
                  borderBottom: '1px solid var(--color-border)',
                  display: 'flex', alignItems: 'center', gap: 6,
                }}
                onMouseEnter={e => { if (selectedSessionId !== session.session_id) e.currentTarget.style.background = 'var(--color-bg-hover)' }}
                onMouseLeave={e => { if (selectedSessionId !== session.session_id) e.currentTarget.style.background = 'transparent' }}
              >
                <FontAwesomeIcon
                  icon={faGlobe}
                  style={{ fontSize: 10, color: 'var(--color-accent)', flexShrink: 0 }}
                />
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{
                    fontSize: 12, fontWeight: selectedSessionId === session.session_id ? 600 : 400,
                    color: 'var(--color-text-primary)',
                    overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
                  }}>
                    {session.site_name || new URL(session.seed_url).hostname}
                  </div>
                  <div style={{ fontSize: 10, color: 'var(--color-text-muted)', marginTop: 1 }}>
                    {session.imported_pages}/{session.total_pages} 頁
                  </div>
                </div>
                <button
                  title="管理"
                  onClick={e => { e.stopPropagation(); setManagingSession(session); setShowManage(true) }}
                  style={{ ...iconBtn, fontSize: 10, opacity: 0.5, padding: '2px 4px' }}
                >
                  ⋯
                </button>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* ── Main Q&A Area ── */}
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden', minWidth: 0 }}>
        {/* Header */}
        <div style={{
          padding: '8px 14px', borderBottom: '1px solid var(--color-border)',
          display: 'flex', alignItems: 'center', gap: 8, flexShrink: 0,
          background: 'var(--color-bg)',
        }}>
          <button
            onClick={() => setShowSidebar(v => !v)}
            title={showSidebar ? '收合來源' : '展開來源'}
            style={iconBtn}
          >
            <FontAwesomeIcon icon={showSidebar ? faChevronLeft : faChevronRight} style={{ fontSize: 11 }} />
          </button>
          <span style={{ fontSize: 13, fontWeight: 600, color: 'var(--color-text-primary)', flex: 1 }}>
            {selectedSession
              ? `${selectedSession.site_name || new URL(selectedSession.seed_url).hostname}`
              : '知識庫問答'}
          </span>
          {selectedSession && (
            <span style={{ fontSize: 11, color: 'var(--color-text-muted)' }}>
              {selectedSession.imported_pages} 頁
            </span>
          )}
          <button
            title="管理來源"
            onClick={() => {
              setManagingSession(selectedSession)
              setShowManage(true)
            }}
            style={iconBtn}
          >
            <FontAwesomeIcon icon={faArrowsRotate} style={{ fontSize: 12 }} />
          </button>
        </div>

        {/* Messages */}
        <div style={{ flex: 1, overflowY: 'auto', padding: '16px 20px', display: 'flex', flexDirection: 'column', gap: 16 }}>
          {messages.length === 0 && (
            <WelcomeScreen sessions={sessions} onSelectSession={setSelectedSessionIdStore} />
          )}
          {messages.map(msg => (
            <MessageBubble
              key={msg.id}
              message={msg}
              onOpenNote={onOpenNote}
            />
          ))}
          <div ref={messagesEndRef} />
        </div>

        {/* Input */}
        <div style={{
          padding: '10px 16px', borderTop: '1px solid var(--color-border)',
          background: 'var(--color-bg)', flexShrink: 0,
        }}>
          <div style={{ display: 'flex', gap: 8, alignItems: 'flex-end' }}>
            <textarea
              ref={inputRef}
              value={input}
              onChange={e => setInput(e.target.value)}
              onKeyDown={e => {
                if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); handleQuery() }
              }}
              placeholder={sessions.length === 0
                ? '請先新增來源並匯入頁面…'
                : `向知識庫提問…${selectedSession ? `（僅查詢 ${selectedSession.site_name || '此來源'}）` : ''}`}
              disabled={isQuerying || sessions.length === 0}
              rows={1}
              style={{
                flex: 1, resize: 'none', minHeight: 36, maxHeight: 120,
                background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
                borderRadius: 8, padding: '8px 12px', fontSize: 13,
                color: 'var(--color-text-primary)', outline: 'none',
                lineHeight: 1.5, fontFamily: 'inherit',
                opacity: (isQuerying || sessions.length === 0) ? 0.6 : 1,
              }}
            />
            <button
              onClick={handleQuery}
              disabled={!input.trim() || isQuerying || sessions.length === 0}
              style={{
                width: 36, height: 36, flexShrink: 0,
                background: 'var(--color-accent)', border: 'none', borderRadius: 8,
                color: '#fff', cursor: 'pointer', display: 'flex', alignItems: 'center', justifyContent: 'center',
                opacity: (!input.trim() || isQuerying || sessions.length === 0) ? 0.5 : 1,
              }}
            >
              <FontAwesomeIcon icon={isQuerying ? faSpinner : faPaperPlane} spin={isQuerying} style={{ fontSize: 13 }} />
            </button>
          </div>
        </div>
      </div>

      {/* ── Source Management Overlay ── */}
      {showManage && (
        <SourceManagePanel
          sessions={sessions}
          focusSession={managingSession}
          onClose={() => { setShowManage(false); setManagingSession(null) }}
          onSessionsChange={loadSessions}
          onDelete={handleDeleteSession}
          addLog={addLog}
          onOpenNote={onOpenNote}
        />
      )}
    </div>
  )
}

// ── Welcome Screen ─────────────────────────────────────────────────────────────

function WelcomeScreen({
  sessions,
  onSelectSession,
}: {
  sessions: ImportSessionSummary[]
  onSelectSession: (id: string | null) => void
}) {
  return (
    <div style={{
      flex: 1, display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center',
      gap: 12, padding: '40px 20px',
    }}>
      <FontAwesomeIcon icon={faDatabase} style={{ fontSize: 32, color: 'var(--color-accent)', opacity: 0.7 }} />
      <div style={{ fontSize: 16, fontWeight: 600, color: 'var(--color-text-primary)' }}>
        知識庫問答
      </div>
      <div style={{ fontSize: 13, color: 'var(--color-text-muted)', textAlign: 'center', lineHeight: 1.6, maxWidth: 340 }}>
        {sessions.length === 0
          ? '尚無知識來源。點擊左側「+」新增網站 URL，匯入頁面後即可開始提問。'
          : '向下方輸入框提問，系統會自動搜尋相關筆記並生成答案。'}
      </div>
      {sessions.length > 0 && (
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6, justifyContent: 'center', marginTop: 4 }}>
          {sessions.slice(0, 4).map(s => (
            <button
              key={s.session_id}
              onClick={() => onSelectSession(s.session_id)}
              style={{
                padding: '4px 10px', borderRadius: 14, fontSize: 11, cursor: 'pointer',
                background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
                color: 'var(--color-text-secondary)',
              }}
            >
              <FontAwesomeIcon icon={faGlobe} style={{ marginRight: 5, fontSize: 10 }} />
              {s.site_name || new URL(s.seed_url).hostname}
              <span style={{ marginLeft: 4, color: 'var(--color-text-muted)' }}>
                {s.imported_pages}p
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

// ── Message Bubble ─────────────────────────────────────────────────────────────

function MessageBubble({ message, onOpenNote }: { message: KBMessage; onOpenNote?: (path: string) => void }) {
  if (message.role === 'user') {
    return (
      <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
        <div style={{
          maxWidth: '72%', padding: '9px 13px',
          background: 'var(--color-accent)', borderRadius: '14px 14px 4px 14px',
          color: '#fff', fontSize: 13, lineHeight: 1.6, whiteSpace: 'pre-wrap',
        }}>
          {message.content}
        </div>
      </div>
    )
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <div style={{
        padding: '10px 14px',
        background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
        borderRadius: '4px 14px 14px 14px',
        fontSize: 13, lineHeight: 1.7, color: 'var(--color-text-primary)',
        whiteSpace: 'pre-wrap', position: 'relative',
      }}>
        {message.content || (message.isStreaming ? '' : '…')}
        {message.isStreaming && (
          <span style={{
            display: 'inline-block', width: 8, height: 13, marginLeft: 2, verticalAlign: 'text-bottom',
            background: 'var(--color-accent)', borderRadius: 2, opacity: 0.8,
            animation: 'blink 1s steps(1) infinite',
          }} />
        )}
        {message.error && (
          <div style={{ marginTop: 6, fontSize: 12, color: 'var(--color-danger, #e06c75)' }}>
            ⚠ {message.error}
          </div>
        )}
      </div>

      {/* Refs */}
      {message.refs && message.refs.length > 0 && (
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 5 }}>
          {message.refs.map((ref, i) => (
            <button
              key={ref.path}
              onClick={() => onOpenNote?.(ref.path)}
              title={ref.excerpt}
              style={{
                display: 'flex', alignItems: 'center', gap: 4,
                padding: '3px 9px', borderRadius: 12,
                background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
                color: 'var(--color-text-secondary)', fontSize: 11, cursor: 'pointer',
              }}
              onMouseEnter={e => (e.currentTarget.style.borderColor = 'var(--color-accent)')}
              onMouseLeave={e => (e.currentTarget.style.borderColor = 'var(--color-border)')}
            >
              <FontAwesomeIcon icon={faFileLines} style={{ fontSize: 10, color: 'var(--color-accent)' }} />
              <span style={{ color: 'var(--color-accent)', fontWeight: 600, marginRight: 2 }}>[{i + 1}]</span>
              <span style={{ maxWidth: 140, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                {ref.title}
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

// ── Source Management Panel (slide-over) ───────────────────────────────────────

interface PageRow {
  page_id: string
  session_id: string
  url: string
  title: string
  parent_url: string | null
  depth: number
  note_path: string | null
  content_hash: string | null
  status: string
  last_crawled: number | null
}

function SourceManagePanel({
  sessions, focusSession, onClose, onSessionsChange, onDelete, addLog, onOpenNote,
}: {
  sessions: ImportSessionSummary[]
  focusSession: ImportSessionSummary | null
  onClose: () => void
  onSessionsChange: () => Promise<void>
  onDelete: (id: string) => void
  addLog: (cat: string, level: 'info' | 'warn' | 'error', msg: string) => void
  onOpenNote?: (path: string) => void
}) {
  const [activeSession, setActiveSession] = useState<ImportSessionSummary | null>(focusSession)
  const [pages, setPages] = useState<PageRow[]>([])
  const [loadingPages, setLoadingPages] = useState(false)
  const [analyzingSession, setAnalyzingSession] = useState<string | null>(null)
  const [importingIds, setImportingIds] = useState<Set<string>>(new Set())
  // AI KB card suggestions
  const [suggestingPageId, setSuggestingPageId] = useState<string | null>(null)
  const [suggestions, setSuggestions] = useState<KBCardSuggestion[]>([])
  const [creatingCardTitle, setCreatingCardTitle] = useState<string | null>(null)
  // Batch verify state
  const [noteStatuses, setNoteStatuses] = useState<Record<string, string>>({})
  const [verifyMode, setVerifyMode] = useState(false)
  const [verifyIdx, setVerifyIdx] = useState(0)

  useEffect(() => {
    if (activeSession) {
      setLoadingPages(true)
      invoke<PageRow[]>('get_session_pages', { sessionId: activeSession.session_id })
        .then(r => setPages(r))
        .catch(e => addLog('import', 'error', `載入頁面失敗：${fmtError(e)}`))
        .finally(() => setLoadingPages(false))
    } else {
      setPages([])
    }
  }, [activeSession])

  const handleAnalyze = async (session: ImportSessionSummary) => {
    setAnalyzingSession(session.session_id)
    try {
      const result = await invoke<PageRow[]>('fetch_site_outline', { sessionId: session.session_id })
      setPages(result)
      await onSessionsChange()
      toast.success(`已發現 ${result.length} 個頁面`)
    } catch (e: unknown) {
      const msg = fmtError(e)
      addLog('import', 'error', `分析失敗：${msg}`)
      toast.error(msg || '分析失敗')
    } finally {
      setAnalyzingSession(null)
    }
  }

  const handleImportPage = async (page: PageRow) => {
    setImportingIds(s => new Set(s).add(page.page_id))
    setSuggestions([])
    try {
      await invoke('import_page', { sessionId: page.session_id, pageId: page.page_id })
      setPages(prev => prev.map(p => p.page_id === page.page_id ? { ...p, status: 'imported' } : p))
      await onSessionsChange()
      // 非同步觸發 AI 建議（不阻塞匯入流程）
      setSuggestingPageId(page.page_id)
      invoke<KBCardSuggestion[]>('suggest_kb_cards', {
        sessionId: page.session_id,
        pageId: page.page_id,
      }).then(cards => {
        if (cards.length > 0) setSuggestions(cards)
      }).catch(() => { /* 建議失敗不影響匯入 */ }).finally(() => {
        setSuggestingPageId(null)
      })
    } catch (e: unknown) {
      const msg = fmtError(e)
      addLog('import', 'error', `匯入失敗：${msg}`)
      toast.error(msg || '匯入失敗')
      setPages(prev => prev.map(p => p.page_id === page.page_id ? { ...p, status: 'failed' } : p))
    } finally {
      setImportingIds(s => { const n = new Set(s); n.delete(page.page_id); return n })
    }
  }

  const handleImportAll = async () => {
    if (!activeSession) return
    const pending = pages.filter(p => p.status === 'pending' || p.status === 'failed')
    if (pending.length === 0) { toast.error('沒有待匯入的頁面'); return }
    for (const p of pending) await handleImportPage(p)
  }

  const handleSetNoteStatus = async (page: PageRow, status: 'verified' | 'deprecated') => {
    if (!page.note_path) return
    try {
      await invoke('set_note_status', { path: page.note_path, status })
      setNoteStatuses(prev => ({ ...prev, [page.page_id]: status }))
    } catch (e) {
      toast.error(`設定狀態失敗：${fmtError(e)}`)
    }
  }

  const handleCreateCard = async (card: KBCardSuggestion) => {
    setCreatingCardTitle(card.title)
    try {
      const path = await invoke<string>('create_note', { title: card.title, content: card.content })
      toast.success(`已建立「${card.title}」`)
      setSuggestions(prev => prev.filter(s => s.title !== card.title))
      onOpenNote?.(path)
    } catch (e) {
      toast.error(`建立筆記失敗：${fmtError(e)}`)
    } finally {
      setCreatingCardTitle(null)
    }
  }

  // Pages that are imported (have note_path) — candidates for verification
  const importedPages = pages.filter(p => p.status === 'imported' && p.note_path)
  // Current verify target page
  const currentVerifyPage = importedPages[verifyIdx] ?? null
  const verifyTotal = importedPages.length
  const verifiedCount = importedPages.filter(p =>
    noteStatuses[p.page_id] === 'verified' || noteStatuses[p.page_id] === 'deprecated'
  ).length

  const advanceVerify = () => {
    setVerifyIdx(i => Math.min(i + 1, verifyTotal - 1))
  }

  return (
    <div style={{
      position: 'absolute', inset: 0, display: 'flex', flexDirection: 'column',
      background: 'var(--color-bg-base)', zIndex: 50,
    }}>
      {/* Header */}
      <div style={{
        padding: '10px 14px', borderBottom: '1px solid var(--color-border)',
        display: 'flex', alignItems: 'center', gap: 8, flexShrink: 0,
        background: 'var(--color-bg)',
      }}>
        {activeSession && (
          <button onClick={() => setActiveSession(null)} style={iconBtn}>
            <FontAwesomeIcon icon={faChevronLeft} style={{ fontSize: 12 }} />
          </button>
        )}
        <span style={{ fontSize: 13, fontWeight: 600, flex: 1, color: 'var(--color-text-primary)' }}>
          {activeSession ? (activeSession.site_name || activeSession.seed_url) : '管理來源'}
        </span>
        {activeSession && (
          <>
            <button
              title="分析網站頁面"
              disabled={!!analyzingSession}
              onClick={() => handleAnalyze(activeSession)}
              style={iconBtn}
            >
              <FontAwesomeIcon icon={analyzingSession ? faSpinner : faGlobe} spin={!!analyzingSession} />
            </button>
            <button
              title="全部匯入"
              onClick={handleImportAll}
              style={iconBtn}
            >
              <FontAwesomeIcon icon={faDownload} />
            </button>
            {importedPages.length > 0 && (
              <button
                title={`批次驗證（${importedPages.length} 頁已匯入）`}
                onClick={() => { setVerifyMode(true); setVerifyIdx(0) }}
                style={{ ...iconBtn, color: 'var(--color-accent)' }}
              >
                <FontAwesomeIcon icon={faListCheck} />
              </button>
            )}
          </>
        )}
        <button onClick={onClose} style={{ ...iconBtn, marginLeft: 4 }}>✕</button>
      </div>

      {/* Body */}
      {!activeSession ? (
        /* Sessions list */
        <div style={{ flex: 1, overflowY: 'auto' }}>
          {sessions.length === 0 ? (
            <div style={{ padding: 24, textAlign: 'center', color: 'var(--color-text-muted)', fontSize: 12 }}>
              尚無來源
            </div>
          ) : sessions.map(s => (
            <div
              key={s.session_id}
              style={{
                padding: '10px 14px', borderBottom: '1px solid var(--color-border)',
                display: 'flex', alignItems: 'center', gap: 8, cursor: 'pointer',
              }}
              onClick={() => setActiveSession(s)}
              onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
              onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
            >
              <FontAwesomeIcon icon={faGlobe} style={{ color: 'var(--color-accent)', fontSize: 13 }} />
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontSize: 13, fontWeight: 500, color: 'var(--color-text-primary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {s.site_name || s.seed_url}
                </div>
                <div style={{ fontSize: 11, color: 'var(--color-text-muted)', marginTop: 1 }}>
                  {s.imported_pages}/{s.total_pages} 頁 · {s.seed_url}
                </div>
              </div>
              <label
                title="自動更新（啟動時偵測變更）"
                onClick={e => e.stopPropagation()}
                style={{ display: 'flex', alignItems: 'center', gap: 4, cursor: 'pointer', fontSize: 11, color: 'var(--color-text-muted)' }}
              >
                <input
                  type="checkbox"
                  checked={s.auto_update ?? false}
                  onChange={async e => {
                    const v = e.target.checked
                    try {
                      await invoke('set_session_auto_update', { sessionId: s.session_id, autoUpdate: v })
                      await onSessionsChange()
                    } catch { /* ignore */ }
                  }}
                  style={{ accentColor: 'var(--color-accent)', cursor: 'pointer' }}
                />
                自動
              </label>
              <button
                title="刪除來源"
                onClick={e => { e.stopPropagation(); onDelete(s.session_id) }}
                style={{ ...iconBtn, color: 'var(--color-danger, #e06c75)' }}
              >
                <FontAwesomeIcon icon={faTrash} style={{ fontSize: 11 }} />
              </button>
            </div>
          ))}
        </div>
      ) : verifyMode ? (
        /* ── Batch Verify Mode ── */
        <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
          {/* Progress bar */}
          <div style={{ padding: '8px 14px 0', flexShrink: 0 }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 11, color: 'var(--color-text-muted)', marginBottom: 4 }}>
              <span>已處理 {verifiedCount} / {verifyTotal}</span>
              <button onClick={() => setVerifyMode(false)} style={{ ...iconBtn, fontSize: 11, padding: '0 4px' }}>
                返回列表
              </button>
            </div>
            <div style={{ height: 3, borderRadius: 2, background: 'var(--color-border)', overflow: 'hidden' }}>
              <div style={{
                height: '100%', borderRadius: 2,
                width: `${verifyTotal > 0 ? verifiedCount / verifyTotal * 100 : 0}%`,
                background: 'var(--color-accent)', transition: 'width 0.2s ease',
              }} />
            </div>
          </div>

          {currentVerifyPage ? (
            <div style={{ flex: 1, display: 'flex', flexDirection: 'column', padding: '16px 14px', gap: 12 }}>
              {/* Page info */}
              <div style={{
                padding: '12px 14px', borderRadius: 8, background: 'var(--color-bg-elevated)',
                border: '1px solid var(--color-border)',
              }}>
                <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--color-text-primary)', marginBottom: 4 }}>
                  {currentVerifyPage.title || currentVerifyPage.url}
                </div>
                <div style={{ fontSize: 11, color: 'var(--color-text-muted)', wordBreak: 'break-all' }}>
                  {currentVerifyPage.note_path}
                </div>
                {noteStatuses[currentVerifyPage.page_id] && (
                  <div style={{
                    marginTop: 8, fontSize: 11, fontWeight: 600,
                    color: noteStatuses[currentVerifyPage.page_id] === 'verified'
                      ? 'var(--color-success)' : 'var(--color-text-muted)',
                  }}>
                    {noteStatuses[currentVerifyPage.page_id] === 'verified' ? '✓ 已驗證' : '✗ 已棄用'}
                  </div>
                )}
              </div>

              {/* Action buttons */}
              <div style={{ display: 'flex', gap: 8 }}>
                <button
                  onClick={async () => {
                    await handleSetNoteStatus(currentVerifyPage, 'verified')
                    advanceVerify()
                  }}
                  style={{
                    flex: 1, padding: '9px', borderRadius: 7, border: 'none', cursor: 'pointer',
                    background: 'color-mix(in srgb, var(--color-success) 15%, transparent)',
                    color: 'var(--color-success)', fontWeight: 600, fontSize: 13,
                  }}
                >
                  ✓ 驗證
                </button>
                <button
                  onClick={async () => {
                    await handleSetNoteStatus(currentVerifyPage, 'deprecated')
                    advanceVerify()
                  }}
                  style={{
                    flex: 1, padding: '9px', borderRadius: 7, border: 'none', cursor: 'pointer',
                    background: 'color-mix(in srgb, var(--color-text-muted) 12%, transparent)',
                    color: 'var(--color-text-muted)', fontWeight: 600, fontSize: 13,
                  }}
                >
                  ✗ 棄用
                </button>
                <button
                  onClick={advanceVerify}
                  style={{
                    flex: 1, padding: '9px', borderRadius: 7, border: '1px solid var(--color-border)',
                    cursor: 'pointer', background: 'transparent',
                    color: 'var(--color-text-secondary)', fontSize: 13,
                  }}
                >
                  → 跳過
                </button>
              </div>

              {/* Navigation */}
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                <button
                  disabled={verifyIdx === 0}
                  onClick={() => setVerifyIdx(i => Math.max(0, i - 1))}
                  style={{ ...iconBtn, opacity: verifyIdx === 0 ? 0.3 : 1 }}
                >
                  <FontAwesomeIcon icon={faChevronLeft} style={{ fontSize: 11 }} /> 上一頁
                </button>
                <span style={{ fontSize: 11, color: 'var(--color-text-muted)' }}>
                  {verifyIdx + 1} / {verifyTotal}
                </span>
                <button
                  disabled={verifyIdx >= verifyTotal - 1}
                  onClick={advanceVerify}
                  style={{ ...iconBtn, opacity: verifyIdx >= verifyTotal - 1 ? 0.3 : 1 }}
                >
                  下一頁 <FontAwesomeIcon icon={faChevronRight} style={{ fontSize: 11 }} />
                </button>
              </div>
            </div>
          ) : (
            <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center',
              flexDirection: 'column', gap: 10, color: 'var(--color-text-muted)', fontSize: 13 }}>
              <FontAwesomeIcon icon={faCheckCircle} style={{ fontSize: 28, color: 'var(--color-success)' }} />
              <div>所有頁面已處理完畢</div>
            </div>
          )}
        </div>
      ) : (
        /* Pages list */
        <div style={{ flex: 1, overflowY: 'auto' }}>
          {loadingPages ? (
            <div style={{ padding: 20, textAlign: 'center', color: 'var(--color-text-muted)', fontSize: 12 }}>
              <FontAwesomeIcon icon={faSpinner} spin /> 載入中…
            </div>
          ) : pages.length === 0 ? (
            <div style={{ padding: 24, textAlign: 'center', color: 'var(--color-text-muted)', fontSize: 12, lineHeight: 1.6 }}>
              尚無頁面。點擊「🌐」分析網站來源後再匯入。
            </div>
          ) : pages.map(p => (
            <div
              key={p.page_id}
              style={{
                padding: '7px 14px 7px ' + (14 + p.depth * 12) + 'px',
                borderBottom: '1px solid var(--color-border)',
                display: 'flex', alignItems: 'center', gap: 8,
              }}
            >
              <StatusDot status={p.status} importing={importingIds.has(p.page_id)} />
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontSize: 12, color: 'var(--color-text-primary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {p.title || p.url}
                </div>
              </div>
              {(p.status === 'pending' || p.status === 'failed') && (
                <button
                  disabled={importingIds.has(p.page_id)}
                  onClick={() => handleImportPage(p)}
                  style={{ ...iconBtn, fontSize: 11, color: 'var(--color-accent)' }}
                  title="匯入此頁"
                >
                  {importingIds.has(p.page_id)
                    ? <FontAwesomeIcon icon={faSpinner} spin />
                    : <FontAwesomeIcon icon={faDownload} />}
                </button>
              )}
              {p.status === 'imported' && p.note_path && (() => {
                const ns = noteStatuses[p.page_id]
                if (ns === 'verified') return (
                  <span style={{ fontSize: 10, color: 'var(--color-success)', fontWeight: 600 }}>✓</span>
                )
                if (ns === 'deprecated') return (
                  <span style={{ fontSize: 10, color: 'var(--color-text-muted)', fontWeight: 600 }}>✗</span>
                )
                return (
                  <div style={{ display: 'flex', gap: 3 }}>
                    <button
                      onClick={() => handleSetNoteStatus(p, 'verified')}
                      style={{ ...iconBtn, fontSize: 10, padding: '2px 5px', color: 'var(--color-success)' }}
                      title="標記為已驗證"
                    >✓</button>
                    <button
                      onClick={() => handleSetNoteStatus(p, 'deprecated')}
                      style={{ ...iconBtn, fontSize: 10, padding: '2px 5px', color: 'var(--color-text-muted)' }}
                      title="標記為已棄用"
                    >✗</button>
                  </div>
                )
              })()}
            </div>
          ))}
        </div>
      )}

      {/* AI KB card suggestions */}
      {(suggestingPageId !== null || suggestions.length > 0) && (
        <div style={{
          borderTop: '1px solid var(--color-border)',
          background: 'var(--color-bg-secondary)',
          padding: '10px 14px',
          flexShrink: 0,
        }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
            <div style={{ fontSize: 12, fontWeight: 600, color: 'var(--color-text-secondary)', display: 'flex', alignItems: 'center', gap: 6 }}>
              {suggestingPageId !== null && <FontAwesomeIcon icon={faSpinner} spin style={{ fontSize: 10 }} />}
              🤖 AI 知識卡片建議
            </div>
            {suggestions.length > 0 && (
              <button
                onClick={() => setSuggestions([])}
                style={{ ...iconBtn, fontSize: 10, padding: '2px 5px' }}
                title="關閉建議"
              >✕</button>
            )}
          </div>
          {suggestions.map((card) => {
            const templateLabel = card.template === 'concept' ? '概念' : card.template === 'procedure' ? '步驟' : '參考'
            const templateColor = card.template === 'concept' ? 'var(--color-accent)' : card.template === 'procedure' ? 'var(--color-success)' : 'var(--color-warning, #e5a50a)'
            return (
              <div
                key={card.title}
                style={{
                  background: 'var(--color-bg-primary)',
                  border: '1px solid var(--color-border)',
                  borderRadius: 6,
                  padding: '8px 10px',
                  marginBottom: 6,
                  display: 'flex',
                  alignItems: 'flex-start',
                  gap: 8,
                }}
              >
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 3 }}>
                    <span style={{
                      fontSize: 10, fontWeight: 700, padding: '1px 5px',
                      borderRadius: 3, background: templateColor + '22',
                      color: templateColor, flexShrink: 0,
                    }}>{templateLabel}</span>
                    <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--color-text-primary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                      {card.title}
                    </span>
                  </div>
                  <div style={{ fontSize: 11, color: 'var(--color-text-muted)', lineHeight: 1.4 }}>{card.reason}</div>
                </div>
                <button
                  disabled={creatingCardTitle === card.title}
                  onClick={() => handleCreateCard(card)}
                  style={{ ...iconBtn, fontSize: 11, padding: '3px 8px', color: 'var(--color-accent)', flexShrink: 0, whiteSpace: 'nowrap' }}
                  title="建立筆記"
                >
                  {creatingCardTitle === card.title
                    ? <FontAwesomeIcon icon={faSpinner} spin />
                    : '+ 建立'}
                </button>
              </div>
            )
          })}
          {suggestingPageId !== null && suggestions.length === 0 && (
            <div style={{ fontSize: 11, color: 'var(--color-text-muted)', textAlign: 'center', padding: '4px 0' }}>
              AI 正在分析知識點…
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function StatusDot({ status, importing }: { status: string; importing: boolean }) {
  if (importing) return <FontAwesomeIcon icon={faSpinner} spin style={{ fontSize: 9, color: 'var(--color-accent)', flexShrink: 0 }} />
  const color = status === 'imported' ? 'var(--color-success, #4caf50)'
    : status === 'failed' ? 'var(--color-danger, #e06c75)'
    : 'var(--color-text-muted)'
  return <div style={{ width: 7, height: 7, borderRadius: '50%', background: color, flexShrink: 0 }} />
}

// ── Styles ─────────────────────────────────────────────────────────────────────

const iconBtn: React.CSSProperties = {
  background: 'none', border: 'none', cursor: 'pointer',
  color: 'var(--color-text-muted)', padding: '4px 6px',
  borderRadius: 4, fontSize: 13, display: 'flex', alignItems: 'center', justifyContent: 'center',
}
