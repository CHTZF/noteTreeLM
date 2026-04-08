import { useState, useCallback, useRef, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { api } from '../../lib/api'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import { faSpinner, faPaperPlane, faBook, faFileLines, faShieldHalved, faChevronLeft, faChevronRight } from '@fortawesome/free-solid-svg-icons'
import { type KnowledgeRef } from '../../stores/knowledgeChatStore'
import ConversationList from '../Chat/ConversationList'
import { useAuthStore } from '../../stores/authStore'
import { useSettingsStore } from '../../stores/settingsStore'

// ── Types ──────────────────────────────────────────────────────────────────────

interface KBAssistMessage {
  id: string
  role: 'user' | 'assistant'
  content: string
  refs?: KnowledgeRef[]
  isStreaming?: boolean
  error?: string
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

const KB_MODE = 'kb_assist' as const

// ── Main Component ─────────────────────────────────────────────────────────────

export default function KnowledgeAssistant({ onOpenNote }: Props) {
  const currentVaultId = useSettingsStore(s => s.currentVaultId)
  const [messages, setMessages] = useState<KBAssistMessage[]>([])
  const [input, setInput] = useState('')
  const [isQuerying, setIsQuerying] = useState(false)
  const [conversationId, setConversationId] = useState<string | null>(null)
  const [sidebarOpen, setSidebarOpen] = useState(true)
  const pendingRefsRef = useRef<KnowledgeRef[]>([])
  const messagesEndRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const messagesRef = useRef<KBAssistMessage[]>([])

  // Keep messagesRef in sync for saving without stale closure issues
  useEffect(() => { messagesRef.current = messages }, [messages])

  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  // ── Conversation persistence ───────────────────────────────────────────────

  const saveCurrentConversation = useCallback(async (convId: string, msgs: KBAssistMessage[]) => {
    if (!convId || msgs.length === 0) return
    const saveable = msgs
      .filter(m => !m.isStreaming)
      .map(m => ({ role: m.role, content: m.content, refs: m.refs }))
    if (saveable.length === 0) return
    try {
      await api.saveConversationMessages(convId, saveable)
    } catch { /* non-fatal */ }
  }, [])

  const loadConversationMessages = useCallback(async (id: string): Promise<KBAssistMessage[]> => {
    try {
      const snap = await api.getConversation(id) as { messages_json: string }
      const raw = JSON.parse(snap.messages_json) as Array<{ role: string; content: string; refs?: KnowledgeRef[] }>
      return raw.map(m => ({
        id: crypto.randomUUID(),
        role: m.role as 'user' | 'assistant',
        content: m.content,
        refs: m.refs,
      }))
    } catch {
      return []
    }
  }, [])

  const createNewConversation = useCallback(async (): Promise<string> => {
    const username = useAuthStore.getState().session?.username ?? ''
    const vaultId = await invoke<string>('get_vault_uuid')
    const result = await api.createConversation({ account_id: username, mode: KB_MODE, vault_id: vaultId })
    return result.id
  }, [])

  // ── Mount: restore last KB conversation ───────────────────────────────────

  useEffect(() => {
    if (!currentVaultId) return
    ;(async () => {
      try {
        const username = useAuthStore.getState().session?.username ?? ''
        const lastId = await api.getLastModeConversationId(`${username}_${currentVaultId}`, KB_MODE as string).catch(() => null)
        if (lastId) {
          const msgs = await loadConversationMessages(lastId)
          if (msgs.length > 0) {
            setConversationId(lastId)
            setMessages(msgs)
            return
          }
        }
        // No usable last conversation — create a fresh one
        const newId = await createNewConversation()
        setConversationId(newId)
        setMessages([])
      } catch { /* start empty */ }
    })()
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [currentVaultId])

  // ── Conversation list handlers ────────────────────────────────────────────

  const handleSelectConversation = useCallback(async (id: string) => {
    if (id === conversationId) return
    await saveCurrentConversation(conversationId ?? '', messagesRef.current)
    const msgs = await loadConversationMessages(id)
    setConversationId(id)
    setMessages(msgs)
    invoke<string>('get_vault_uuid').then(vaultId => {
      const username = useAuthStore.getState().session?.username ?? ''
      api.setLastModeConversationId(`${username}_${vaultId}`, KB_MODE as string, id).catch(() => {})
    }).catch(() => {})
  }, [conversationId, saveCurrentConversation, loadConversationMessages])

  const handleNewConversation = useCallback(async (id: string) => {
    await saveCurrentConversation(conversationId ?? '', messagesRef.current)
    if (!id) {
      // ConversationList deleted the current one → create fresh
      try { id = await createNewConversation() } catch { return }
    }
    setConversationId(id)
    setMessages([])
    invoke<string>('get_vault_uuid').then(vaultId => {
      const username = useAuthStore.getState().session?.username ?? ''
      api.setLastModeConversationId(`${username}_${vaultId}`, KB_MODE as string, id).catch(() => {})
    }).catch(() => {})
  }, [conversationId, saveCurrentConversation, createNewConversation])

  // ── Query ─────────────────────────────────────────────────────────────────

  const handleQuery = useCallback(async () => {
    const q = input.trim()
    if (!q || isQuerying) return

    // Ensure we have a conversation
    let convId = conversationId
    if (!convId) {
      try { convId = await createNewConversation(); setConversationId(convId) } catch { return }
    }

    const queryId = crypto.randomUUID()
    const assistantMsgId = crypto.randomUUID()
    setMessages(prev => [
      ...prev,
      { id: crypto.randomUUID(), role: 'user', content: q },
      { id: assistantMsgId, role: 'assistant', content: '', isStreaming: true },
    ])
    setInput('')
    setIsQuerying(true)
    pendingRefsRef.current = []

    const unToken = await listen<{ session_id: string; content: string }>('llm:token', e => {
      if (e.payload.session_id !== queryId) return
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
    unDone = await listen<{ session_id: string; error?: string }>('llm:done', e => {
      if (e.payload.session_id !== queryId) return
      const currentRefs = pendingRefsRef.current
      pendingRefsRef.current = []
      setMessages(prev => {
        const updated = prev.map(m =>
          m.id === assistantMsgId
            ? { ...m, isStreaming: false, error: e.payload.error, refs: currentRefs }
            : m
        )
        // Save after state update (use updated directly, not messagesRef)
        if (convId) {
          const saveable = updated
            .filter(m => !m.isStreaming)
            .map(m => ({ role: m.role, content: m.content, refs: m.refs }))
          api.saveConversationMessages(convId, saveable).catch(() => {})
        }
        return updated
      })
      setIsQuerying(false)
      unToken(); unRefs(); unDone?.()
    })

    try {
      await invoke('query_kb', { queryId, question: q })
    } catch (e: unknown) {
      const msg = fmtError(e)
      setMessages(prev => prev.map(m =>
        m.id === assistantMsgId ? { ...m, isStreaming: false, error: msg } : m
      ))
      setIsQuerying(false)
      unToken(); unRefs(); unDone?.()
    }
  }, [input, isQuerying, conversationId, createNewConversation])

  // ── Render ────────────────────────────────────────────────────────────────

  return (
    <div style={{ display: 'flex', height: '100%', background: 'var(--color-bg-base)', overflow: 'hidden' }}>

      {/* Sidebar */}
      {sidebarOpen && (
        <div style={{
          width: 220, flexShrink: 0,
          borderRight: '1px solid var(--color-border)',
          display: 'flex', flexDirection: 'column',
          background: 'var(--color-bg)',
          overflow: 'hidden',
        }}>
          <ConversationList
            mode={KB_MODE}
            selectedId={conversationId}
            onSelect={handleSelectConversation}
            onNew={handleNewConversation}
          />
        </div>
      )}

      {/* Main content */}
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>

        {/* Header */}
        <div style={{
          padding: '10px 16px', borderBottom: '1px solid var(--color-border)',
          display: 'flex', alignItems: 'center', gap: 8, flexShrink: 0,
          background: 'var(--color-bg)',
        }}>
          <button
            onClick={() => setSidebarOpen(o => !o)}
            title={sidebarOpen ? '收起側欄' : '展開側欄'}
            style={{ color: 'var(--color-text-muted)', background: 'none', border: 'none', cursor: 'pointer', padding: '2px 4px', fontSize: 13 }}
          >
            <FontAwesomeIcon icon={sidebarOpen ? faChevronLeft : faChevronRight} />
          </button>
          <FontAwesomeIcon icon={faShieldHalved} style={{ fontSize: 13, color: 'var(--color-accent)' }} />
          <span style={{ fontSize: 13, fontWeight: 600, color: 'var(--color-text-primary)', flex: 1 }}>
            知識庫助手
          </span>
          <span style={{ fontSize: 11, color: 'var(--color-text-muted)' }}>
            僅使用已驗證知識 · 附來源引用
          </span>
          <button
            onClick={async () => {
              try {
                const result = await invoke<string>('debug_kb_chunks')
                alert(result)
              } catch (e) { alert(String(e)) }
            }}
            title="診斷：查看 chunks 狀態"
            style={{ fontSize: 11, padding: '2px 6px', borderRadius: 4, border: '1px solid var(--color-border)', background: 'transparent', color: 'var(--color-text-muted)', cursor: 'pointer' }}
          >
            診斷
          </button>
        </div>

        {/* Messages */}
        <div style={{ flex: 1, overflowY: 'auto', padding: '16px 20px', display: 'flex', flexDirection: 'column', gap: 16 }}>
          {messages.length === 0 && <WelcomeScreen />}
          {messages.map(msg => (
            <KBMessage key={msg.id} message={msg} onOpenNote={onOpenNote} />
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
              placeholder="向知識庫提問（只使用已驗證筆記回答）…"
              disabled={isQuerying}
              rows={1}
              style={{
                flex: 1, resize: 'none', minHeight: 36, maxHeight: 120,
                background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
                borderRadius: 8, padding: '8px 12px', fontSize: 13,
                color: 'var(--color-text-primary)', outline: 'none',
                lineHeight: 1.5, fontFamily: 'inherit',
                opacity: isQuerying ? 0.6 : 1,
              }}
            />
            <button
              onClick={handleQuery}
              disabled={!input.trim() || isQuerying}
              style={{
                width: 36, height: 36, flexShrink: 0,
                background: 'var(--color-accent)', border: 'none', borderRadius: 8,
                color: '#fff', cursor: 'pointer', display: 'flex', alignItems: 'center', justifyContent: 'center',
                opacity: (!input.trim() || isQuerying) ? 0.5 : 1,
              }}
            >
              <FontAwesomeIcon icon={isQuerying ? faSpinner : faPaperPlane} spin={isQuerying} style={{ fontSize: 13 }} />
            </button>
          </div>
        </div>
      </div>
    </div>
  )
}

// ── Welcome Screen ─────────────────────────────────────────────────────────────

function WelcomeScreen() {
  return (
    <div style={{
      flex: 1, display: 'flex', flexDirection: 'column', alignItems: 'center',
      justifyContent: 'center', gap: 12, padding: '40px 20px',
    }}>
      <FontAwesomeIcon icon={faBook} style={{ fontSize: 36, color: 'var(--color-accent)', opacity: 0.7 }} />
      <div style={{ fontSize: 16, fontWeight: 600, color: 'var(--color-text-primary)' }}>
        知識庫助手
      </div>
      <div style={{ fontSize: 13, color: 'var(--color-text-muted)', textAlign: 'center', lineHeight: 1.7, maxWidth: 360 }}>
        此模式嚴格只使用你知識庫中的<strong style={{ color: 'var(--color-text-secondary)' }}>已驗證筆記</strong>回答。<br />
        每個回答都會標示來源，可點擊直接跳到原文段落。<br />
        若知識庫中沒有相關資料，會明確告知而非猜測。
      </div>
    </div>
  )
}

// ── Message ────────────────────────────────────────────────────────────────────

function KBMessage({ message, onOpenNote }: { message: KBAssistMessage; onOpenNote?: (path: string) => void }) {
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
        whiteSpace: 'pre-wrap',
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
              <span style={{ maxWidth: 160, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                {ref.title}
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  )
}
