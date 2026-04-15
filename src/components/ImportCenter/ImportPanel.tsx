import { useState, useEffect, useCallback, useRef } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { api } from '../../lib/api'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import {
  faPlus, faTrash, faPaperPlane, faSpinner, faGlobe,
  faBookmark, faArrowLeft, faExternalLinkAlt, faLightbulb, faXmark,
  faPen, faCheck, faBolt,
} from '@fortawesome/free-solid-svg-icons'
import type { AgentSkill, AgentScope } from '../../types/models'
import SkillStatsPanel from './SkillStats'
import { toast } from '../common/Toast'
import { useKnowledgeChatStore, type KBMessage, type KnowledgeRef } from '../../stores/knowledgeChatStore'
import { useAuthStore } from '../../stores/authStore'
import { useSettingsStore } from '../../stores/settingsStore'

// ── Types ─────────────────────────────────────────────────────────────────────

interface KBCardSuggestion {
  title: string
  template: string
  content: string
  reason: string
}

interface KnowledgeItem {
  item_id: string
  vault_id: string
  session_id: string
  title: string
  source_refs: KnowledgeRef[]
  ai_summary: string
  created_at: number
}

type RightView =
  | { type: 'empty' }
  | { type: 'add_url' }
  | { type: 'chat'; sessionId: string }
  | { type: 'detail'; itemId: string }
  | { type: 'skills_hub' }

function fmtError(e: unknown): string {
  if (typeof e === 'string') return e
  if (e && typeof e === 'object') {
    const obj = e as Record<string, unknown>
    if (typeof obj.message === 'string') return obj.message
    return JSON.stringify(e)
  }
  return String(e)
}

// ── Main Component ────────────────────────────────────────────────────────────

export default function ImportPanel() {
  // ── Knowledge items ─────────────────────────────────────────────────────────
  const [items, setItems] = useState<KnowledgeItem[]>([])
  const [loadingItems, setLoadingItems] = useState(false)

  // ── Right panel view ────────────────────────────────────────────────────────
  const [view, setView] = useState<RightView>({ type: 'empty' })

  // ── URL input state ─────────────────────────────────────────────────────────
  const [urlInput, setUrlInput] = useState('')
  const [analyzingUrl, setAnalyzingUrl] = useState(false)
  const urlInputRef = useRef<HTMLInputElement>(null)

  // ── Chat state ──────────────────────────────────────────────────────────────
  const messagesBySession = useKnowledgeChatStore(s => s.messagesBySession)
  const setMessages = useKnowledgeChatStore(s => s.setMessages)
  const activeKbSessionId = useKnowledgeChatStore(s => s.activeKbSessionId)
  const setActiveKbSessionId = useKnowledgeChatStore(s => s.setActiveKbSessionId)
  const clearMessages = useKnowledgeChatStore(s => s.clearMessages)
  const username = useAuthStore(s => s.session?.username ?? '')
  const currentVaultId = useSettingsStore(s => s.currentVaultId)
  const sessionKey = view.type === 'chat' ? view.sessionId : '_none'
  const messages = messagesBySession[sessionKey] ?? []
  const [chatInput, setChatInput] = useState('')
  const [isQuerying, setIsQuerying] = useState(false)
  const pendingRefsRef = useRef<KnowledgeRef[]>([])
  const messagesEndRef = useRef<HTMLDivElement>(null)
  const chatInputRef = useRef<HTMLTextAreaElement>(null)
  const [savingMsgId, setSavingMsgId] = useState<string | null>(null)

  // ── Rename state ─────────────────────────────────────────────────────────────
  const [renamingItemId, setRenamingItemId] = useState<string | null>(null)
  const [renameValue, setRenameValue] = useState('')
  const renameInputRef = useRef<HTMLInputElement>(null)

  // ── Detail view state ───────────────────────────────────────────────────────
  const [detailItem, setDetailItem] = useState<KnowledgeItem | null>(null)
  const [detailNoteCards, setDetailNoteCards] = useState<KBCardSuggestion[]>([])
  const [detailSkills, setDetailSkills] = useState<AgentSkill[]>([])
  const [loadingNoteCards, setLoadingNoteCards] = useState(false)
  const [loadingSkillCards, setLoadingSkillCards] = useState(false)
  const [noteCardError, setNoteCardError] = useState<string | null>(null)
  const [skillCardError, setSkillCardError] = useState<string | null>(null)

  // ── Load knowledge items ────────────────────────────────────────────────────
  const loadItems = useCallback(async () => {
    setLoadingItems(true)
    try {
      const result = await invoke<KnowledgeItem[]>('list_knowledge_items')  // keep as invoke — no api wrapper needed
      setItems(result)
    } catch { /* non-critical */ } finally {
      setLoadingItems(false)
    }
  }, [])

  useEffect(() => { loadItems() }, [loadItems])

  // Restore active session + messages from DB on mount
  useEffect(() => {
    if (!username) return
    const restoreSession = async (id: string) => {
      setActiveKbSessionId(id)
      setView({ type: 'chat', sessionId: id })
      // Restore messages if not already in Zustand
      const existing = messagesBySession[id]
      if (!existing || existing.length === 0) {
        try {
          const json = await api.getKbChatMessages(id)
          if (json) {
            const msgs: KBMessage[] = JSON.parse(json)
            if (msgs.length > 0) setMessages(id, () => msgs)
          }
        } catch { /* non-critical */ }
      }
    }
    if (activeKbSessionId) {
      restoreSession(activeKbSessionId)
      return
    }
    if (!currentVaultId) return
    api.getLastModeConversationId(`${username}_${currentVaultId}`, 'kb_session')
      .then(id => { if (id) restoreSession(id) })
      .catch(() => {})
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [username, currentVaultId])

  // Listen for web_search background import
  useEffect(() => {
    const unlisten = listen<{ session_id: string }>('import:session_created', () => { loadItems() })
    return () => { unlisten.then(f => f()) }
  }, [loadItems])

  // ── Auto-scroll chat ────────────────────────────────────────────────────────
  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  // ── Persist messages to DB (debounced, skip while streaming) ────────────────
  useEffect(() => {
    if (!activeKbSessionId || messages.length === 0) return
    if (messages.some(m => m.isStreaming)) return
    const timer = setTimeout(() => {
      api.saveKbChatMessages(activeKbSessionId, JSON.stringify(messages)).catch(() => {})
    }, 800)
    return () => clearTimeout(timer)
  }, [messages, activeKbSessionId])

  // Focus URL input when switching to add_url view
  useEffect(() => {
    if (view.type === 'add_url') {
      setTimeout(() => urlInputRef.current?.focus(), 50)
    }
  }, [view.type])

  // ── Add URL → analyze sitemap → enter chat ─────────────────────────────────
  const handleAddUrl = useCallback(async () => {
    const url = urlInput.trim()
    if (!url) return
    setAnalyzingUrl(true)
    try {
      // Create session
      const session = await invoke<{ session_id: string }>('create_import_session', { seedUrl: url })
      const sessionId = session.session_id
      // Fetch sitemap + embed titles (backend handles this)
      await invoke('fetch_site_outline', { sessionId })
      // Import seed page (fast, no embedding)
      const pages = await invoke<Array<{ page_id: string; depth: number }>>('get_session_pages', { sessionId })
      const seed = pages.find(p => p.depth === 0) ?? pages[0]
      if (seed) {
        await invoke('import_page', { sessionId, pageId: seed.page_id })
      }
      setUrlInput('')
      setActiveKbSessionId(sessionId)
      setView({ type: 'chat', sessionId })
      invoke<string>('get_vault_uuid').then(vaultId => api.setLastModeConversationId(`${username}_${vaultId}`, 'kb_session', sessionId).catch(() => {})).catch(() => {})
      toast.success('分析完成，可以開始提問了')
    } catch (e) {
      toast.error(`新增失敗：${fmtError(e)}`)
    } finally {
      setAnalyzingUrl(false)
    }
  }, [urlInput])

  // ── Send chat query ─────────────────────────────────────────────────────────
  const handleQuery = useCallback(async () => {
    if (view.type !== 'chat') return
    const q = chatInput.trim()
    if (!q || isQuerying) return
    const sessionId = view.sessionId

    setChatInput('')
    setIsQuerying(true)
    pendingRefsRef.current = []

    const userMsgId = `u_${Date.now()}`
    const asstMsgId = `a_${Date.now()}`
    const queryId = `qid_${Date.now()}`

    setMessages(sessionId, prev => [
      ...prev,
      { id: userMsgId, role: 'user', content: q },
      { id: asstMsgId, role: 'assistant', content: '', isStreaming: true },
    ])

    let unToken: (() => void) | null = null
    let unRefs: (() => void) | null = null
    let unImporting: (() => void) | null = null
    let unDone: (() => void) | null = null

    try {
      unToken = await listen<{ session_id: string; t: string }>('llm:token', e => {
        if (e.payload.session_id !== queryId) return
        setMessages(sessionId, prev => prev.map(m =>
          m.id === asstMsgId ? { ...m, content: m.content + e.payload.t } : m
        ))
      })

      unRefs = await listen<{ session_id: string; refs: KnowledgeRef[] }>('agent:refs', e => {
        if (e.payload.session_id !== queryId) return
        pendingRefsRef.current = e.payload.refs
        setMessages(sessionId, prev => prev.map(m =>
          m.id === asstMsgId ? { ...m, refs: e.payload.refs } : m
        ))
      })

      unImporting = await listen<{ query_id: string; titles: string[] }>('knowledge:importing_pages', e => {
        if (e.payload.query_id !== queryId) return
        const names = e.payload.titles.slice(0, 3).join('、')
        setMessages(sessionId, prev => prev.map(m =>
          m.id === asstMsgId ? { ...m, content: `🔍 找到相關頁面「${names}」，下載中…\n\n` } : m
        ))
      })

      const unDebug = await listen<{ query_id: string; pages: string[]; import_errors: string[] }>('knowledge:debug', e => {
        if (e.payload.query_id !== queryId) return
        const info = `[debug] pages=${JSON.stringify(e.payload.pages)}\nerrors=${JSON.stringify(e.payload.import_errors)}`
        setMessages(sessionId, prev => prev.map(m =>
          m.id === asstMsgId ? { ...m, content: m.content + '\n\n' + info } : m
        ))
        unDebug()
      })

      unDone = await listen<{ session_id: string; error?: string }>('llm:done', e => {
        if (e.payload.session_id !== queryId) return
        setMessages(sessionId, prev => prev.map(m =>
          m.id === asstMsgId ? { ...m, isStreaming: false, error: e.payload.error } : m
        ))
        setIsQuerying(false)
        unToken?.(); unRefs?.(); unImporting?.(); unDone?.()
      })

      await invoke('query_knowledge', { queryId, question: q, sessionId })
    } catch (e) {
      setMessages(sessionId, prev => prev.map(m =>
        m.id === asstMsgId ? { ...m, isStreaming: false, error: fmtError(e) } : m
      ))
      setIsQuerying(false)
      unToken?.(); unRefs?.(); unImporting?.(); unDone?.()
    }
  }, [view, chatInput, isQuerying, setMessages])

  // ── Save message as knowledge item ─────────────────────────────────────────
  const handleSaveKnowledge = useCallback(async (msg: KBMessage) => {
    if (view.type !== 'chat' || !msg.refs?.length) return
    setSavingMsgId(msg.id)
    try {
      const title = msg.refs[0]?.title || '知識項目'
      const vaultId = await invoke<string>('get_vault_uuid')
      await api.createKnowledgeItem(vaultId, {
        sessionId: view.sessionId,
        title,
        aiSummary: msg.content,
        sourceRefs: msg.refs,
      })
      await loadItems()
      toast.success(`已儲存「${title}」`)
    } catch (e) {
      toast.error(`儲存失敗：${fmtError(e)}`)
    } finally {
      setSavingMsgId(null)
    }
  }, [view, loadItems])

  // ── 筆記卡片：獨立建立 ───────────────────────────────────────────────────────
  const runNoteCards = useCallback(async (item: KnowledgeItem) => {
    setLoadingNoteCards(true)
    setNoteCardError(null)
    const unlisten = await listen<{ item_id: string; note_cards: KBCardSuggestion[] }>(
      'kb:note_cards_ready', e => {
        if (e.payload.item_id !== item.item_id) return
        setDetailNoteCards(e.payload.note_cards)
        setLoadingNoteCards(false)
        unlisten()
      }
    )
    try {
      await invoke('suggest_note_cards_for_item', { itemId: item.item_id })
    } catch (e) {
      setLoadingNoteCards(false)
      setNoteCardError(fmtError(e))
      unlisten()
    }
  }, [])

  // ── 技能規範：獨立建立 ───────────────────────────────────────────────────────
  const runSkillCards = useCallback(async (item: KnowledgeItem) => {
    setLoadingSkillCards(true)
    setSkillCardError(null)
    const unlisten = await listen<{ item_id: string; skill_cards: AgentSkill[] }>(
      'kb:skill_cards_ready', e => {
        if (e.payload.item_id !== item.item_id) return
        setDetailSkills(e.payload.skill_cards)
        setLoadingSkillCards(false)
        unlisten()
        window.dispatchEvent(new CustomEvent('skills-changed'))
      }
    )
    try {
      await invoke('suggest_skill_cards_for_item', { itemId: item.item_id })
    } catch (e) {
      setLoadingSkillCards(false)
      setSkillCardError(fmtError(e))
      unlisten()
    }
  }, [])

  const handleOpenDetail = useCallback(async (item: KnowledgeItem) => {
    setDetailItem(item)
    setDetailNoteCards([])
    setDetailSkills([])
    setNoteCardError(null)
    setSkillCardError(null)
    setView({ type: 'detail', itemId: item.item_id })

    // 同步載入該知識項目已存在的 skills（前次產生的）
    api.listAgentSkills()
      .then(skills => setDetailSkills(skills as AgentSkill[]))
      .catch(() => {})
  }, [])

  // ── End active session ──────────────────────────────────────────────────────
  const handleEndSession = useCallback(() => {
    if (activeKbSessionId) {
      clearMessages(activeKbSessionId)
      api.saveKbChatMessages(activeKbSessionId, '[]').catch(() => {})
    }
    setActiveKbSessionId(null)
    invoke<string>('get_vault_uuid').then(vaultId => api.setLastModeConversationId(`${username}_${vaultId}`, 'kb_session', null).catch(() => {})).catch(() => {})
    setView({ type: 'empty' })
  }, [activeKbSessionId, clearMessages, setActiveKbSessionId, username])

  // ── Debug: dump DB state ────────────────────────────────────────────────────
  const handleDebug = useCallback(async () => {
    try {
      const result = await invoke<string>('debug_kb_chunks')
      alert(result)
    } catch (e) {
      alert(`debug error: ${fmtError(e)}`)
    }
  }, [])

  // ── Delete knowledge item ───────────────────────────────────────────────────
  const handleDeleteItem = useCallback(async (item: KnowledgeItem, e: React.MouseEvent) => {
    e.stopPropagation()
    try {
      await api.deleteKnowledgeItem(item.item_id)
      await loadItems()
      if (view.type === 'detail' && view.itemId === item.item_id) {
        setView({ type: 'empty' })
      }
    } catch (err) {
      toast.error(`刪除失敗：${fmtError(err)}`)
    }
  }, [view, loadItems])

  // ── Rename knowledge item ───────────────────────────────────────────────────
  const startRename = useCallback((item: KnowledgeItem, e: React.MouseEvent) => {
    e.stopPropagation()
    setRenamingItemId(item.item_id)
    setRenameValue(item.title)
    setTimeout(() => renameInputRef.current?.focus(), 30)
  }, [])

  const commitRename = useCallback(async (item: KnowledgeItem) => {
    const title = renameValue.trim()
    if (!title || title === item.title) { setRenamingItemId(null); return }
    try {
      await api.renameKnowledgeItem(item.item_id, title)
      await loadItems()
      if (detailItem?.item_id === item.item_id) setDetailItem(d => d ? { ...d, title } : d)
    } catch (err) {
      toast.error(`重新命名失敗：${fmtError(err)}`)
    } finally {
      setRenamingItemId(null)
    }
  }, [renameValue, loadItems, detailItem])

  // ── Compute saved source paths (for 已儲存 indicator) ───────────────────────
  const savedSourcePaths = new Set(items.flatMap(it => it.source_refs.map(r => r.path)))

  // ── Render ──────────────────────────────────────────────────────────────────
  return (
    <div className="import-panel-v2">
      {/* Left: Knowledge List */}
      <div className="import-panel-v2__sidebar">
        <div className="import-panel-v2__sidebar-header">
          <span className="import-panel-v2__sidebar-title">知識列表</span>
          {loadingItems && <FontAwesomeIcon icon={faSpinner} spin className="import-panel-v2__spin" />}
        </div>
        <div className="import-panel-v2__sidebar-items">
          {items.length === 0 && !loadingItems && (
            <div className="import-panel-v2__empty-hint">尚無知識項目</div>
          )}
          {items.map(item => (
            <div
              key={item.item_id}
              className={`import-panel-v2__ki-item ${view.type === 'detail' && view.itemId === item.item_id ? 'active' : ''}`}
              onClick={() => renamingItemId === item.item_id ? undefined : handleOpenDetail(item)}
            >
              <FontAwesomeIcon icon={faBookmark} className="import-panel-v2__ki-icon" />
              {renamingItemId === item.item_id ? (
                <>
                  <input
                    ref={renameInputRef}
                    className="import-panel-v2__ki-rename-input"
                    value={renameValue}
                    onChange={e => setRenameValue(e.target.value)}
                    onClick={e => e.stopPropagation()}
                    onKeyDown={e => {
                      if (e.key === 'Enter') { e.preventDefault(); commitRename(item) }
                      if (e.key === 'Escape') { e.stopPropagation(); setRenamingItemId(null) }
                    }}
                  />
                  <button
                    className="import-panel-v2__ki-rename-ok"
                    onClick={e => { e.stopPropagation(); commitRename(item) }}
                    title="確認"
                  >
                    <FontAwesomeIcon icon={faCheck} />
                  </button>
                </>
              ) : (
                <>
                  <span className="import-panel-v2__ki-title">{item.title}</span>
                  <button
                    className="import-panel-v2__ki-edit"
                    onClick={e => startRename(item, e)}
                    title="重新命名"
                  >
                    <FontAwesomeIcon icon={faPen} />
                  </button>
                  <button
                    className="import-panel-v2__ki-delete"
                    onClick={e => handleDeleteItem(item, e)}
                    title="刪除"
                  >
                    <FontAwesomeIcon icon={faTrash} />
                  </button>
                </>
              )}
            </div>
          ))}
        </div>
      </div>

      {/* Right: Content Area */}
      <div className="import-panel-v2__main">
        {/* Top bar */}
        <div className="import-panel-v2__topbar">
          {(view.type === 'detail' || view.type === 'skills_hub') && (
            <button
              className="import-panel-v2__back-btn"
              onClick={() => activeKbSessionId
                ? setView({ type: 'chat', sessionId: activeKbSessionId })
                : setView({ type: 'empty' })
              }
            >
              <FontAwesomeIcon icon={faArrowLeft} />
              {activeKbSessionId ? ' 返回對談' : ' 返回'}
            </button>
          )}
          <div style={{ flex: 1 }} />
          {!activeKbSessionId && (
            <button
              className="import-panel-v2__add-btn"
              onClick={() => setView({ type: 'add_url' })}
            >
              <FontAwesomeIcon icon={faPlus} /> 新增知識
            </button>
          )}
        </div>

        {/* Empty view */}
        {view.type === 'empty' && (
          <div className="import-panel-v2__empty">
            <FontAwesomeIcon icon={faGlobe} className="import-panel-v2__empty-icon" />
            <p>點選「新增知識」輸入網址開始探索</p>
            <p className="import-panel-v2__empty-sub">或點選左側知識項目查看詳情</p>
          </div>
        )}

        {/* Add URL view */}
        {view.type === 'add_url' && (
          <div className="import-panel-v2__add-url">
            <h3 className="import-panel-v2__add-url-title">新增知識來源</h3>
            <p className="import-panel-v2__add-url-hint">輸入網址，系統將自動分析頁面結構並開始問答</p>
            <div className="import-panel-v2__url-row">
              <input
                ref={urlInputRef}
                className="import-panel-v2__url-input"
                type="url"
                placeholder="https://..."
                value={urlInput}
                onChange={e => setUrlInput(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter' && !analyzingUrl) handleAddUrl() }}
                disabled={analyzingUrl}
              />
              <button
                className="import-panel-v2__url-btn"
                onClick={handleAddUrl}
                disabled={!urlInput.trim() || analyzingUrl}
              >
                {analyzingUrl
                  ? <><FontAwesomeIcon icon={faSpinner} spin /> 分析中…</>
                  : '開始'}
              </button>
            </div>
          </div>
        )}

        {/* Chat view */}
        {view.type === 'chat' && (
          <div className="import-panel-v2__chat">
            <div className="import-panel-v2__chat-messages">
              {messages.length === 0 && (
                <div className="import-panel-v2__chat-hint">
                  <FontAwesomeIcon icon={faGlobe} />
                  <span>頁面已下載完成，可以開始提問</span>
                  <button
                    style={{ marginTop: 8, fontSize: 11, opacity: 0.5, background: 'none', border: '1px solid currentColor', borderRadius: 4, padding: '2px 6px', cursor: 'pointer' }}
                    onClick={handleDebug}
                  >
                    debug DB
                  </button>
                </div>
              )}
              {messages.map(msg => (
                <ChatMessage
                  key={msg.id}
                  msg={msg}
                  onSave={handleSaveKnowledge}
                  saving={savingMsgId === msg.id}
                  isSaved={!!(msg.refs?.some(r => savedSourcePaths.has(r.path)))}
                />
              ))}
              <div ref={messagesEndRef} />
            </div>
            <div className="import-panel-v2__chat-input-row">
              <textarea
                ref={chatInputRef}
                className="import-panel-v2__chat-input"
                placeholder="提問…"
                value={chatInput}
                onChange={e => setChatInput(e.target.value)}
                onKeyDown={e => {
                  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); handleQuery() }
                }}
                disabled={isQuerying}
                rows={2}
              />
              <button
                className="import-panel-v2__chat-send"
                onClick={handleQuery}
                disabled={!chatInput.trim() || isQuerying}
              >
                {isQuerying
                  ? <FontAwesomeIcon icon={faSpinner} spin />
                  : <FontAwesomeIcon icon={faPaperPlane} />}
              </button>
            </div>
            <div className="import-panel-v2__chat-footer">
              <button
                className="import-panel-v2__end-btn"
                onClick={handleEndSession}
                disabled={isQuerying}
              >
                <FontAwesomeIcon icon={faXmark} /> 結束新增
              </button>
            </div>
          </div>
        )}

        {/* Detail view */}
        {view.type === 'detail' && detailItem && (
          <div className="import-panel-v2__detail">
            <h2 className="import-panel-v2__detail-title">{detailItem.title}</h2>

            {/* Source refs */}
            {detailItem.source_refs.length > 0 && (
              <div className="import-panel-v2__detail-refs">
                <span className="import-panel-v2__detail-label">來源</span>
                {detailItem.source_refs.map((ref, i) => (
                  <a
                    key={i}
                    href={ref.path}
                    target="_blank"
                    rel="noreferrer"
                    className="import-panel-v2__detail-ref-link"
                  >
                    <FontAwesomeIcon icon={faExternalLinkAlt} /> {ref.title || ref.path}
                  </a>
                ))}
              </div>
            )}

            {/* 知識貢獻統計 */}
            {detailSkills.length > 0 && (() => {
              const totalTriggers = detailSkills.reduce((s, sk) => s + sk.trigger_count, 0)
              return totalTriggers > 0 ? (
                <div className="import-panel-v2__knowledge-contrib">
                  <FontAwesomeIcon icon={faBolt} />
                  本知識已驅動 <strong>{totalTriggers}</strong> 次對話
                </div>
              ) : null
            })()}

            {/* AI summary */}
            <div className="import-panel-v2__detail-summary">
              <span className="import-panel-v2__detail-label">AI 整理摘要</span>
              <div className="import-panel-v2__detail-summary-body">
                {detailItem.ai_summary}
              </div>
            </div>

            {/* AI 建議筆記卡片 */}
            <div className="import-panel-v2__detail-cards">
              <span className="import-panel-v2__detail-label" style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                <FontAwesomeIcon icon={faLightbulb} /> AI 建議筆記卡片
                {loadingNoteCards && <FontAwesomeIcon icon={faSpinner} spin style={{ marginLeft: 6 }} />}
                {!loadingNoteCards && (
                  <button
                    className="import-panel-v2__retry-btn"
                    style={{ marginLeft: 4 }}
                    onClick={() => detailItem && runNoteCards(detailItem)}
                  >
                    建立
                  </button>
                )}
              </span>
              {noteCardError && (
                <div className="import-panel-v2__suggestion-error">
                  <span>⚠ {noteCardError}</span>
                </div>
              )}
              {detailNoteCards.length > 0 ? (
                <div className="import-panel-v2__detail-cards-body">
                  {detailNoteCards.map((card, i) => (
                    <div key={i} className="import-panel-v2__note-card-row">
                      <span className="import-panel-v2__note-card-template">{card.template}</span>
                      <span className="import-panel-v2__note-card-title">{card.title}</span>
                      <span className="import-panel-v2__note-card-reason">{card.reason}</span>
                    </div>
                  ))}
                </div>
              ) : !loadingNoteCards ? (
                <div className="import-panel-v2__empty-hint">暫無建議</div>
              ) : null}
            </div>

            {/* AI 建議技能規範 */}
            <div className="import-panel-v2__detail-skills">
              <span className="import-panel-v2__detail-label" style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                <FontAwesomeIcon icon={faBolt} /> AI 建議技能規範
                {loadingSkillCards && <FontAwesomeIcon icon={faSpinner} spin style={{ marginLeft: 6 }} />}
                {!loadingSkillCards && (
                  <button
                    className="import-panel-v2__retry-btn"
                    style={{ marginLeft: 4 }}
                    onClick={() => detailItem && runSkillCards(detailItem)}
                  >
                    建立
                  </button>
                )}
              </span>
              {skillCardError && (
                <div className="import-panel-v2__suggestion-error">
                  <span>⚠ {skillCardError}</span>
                </div>
              )}
              {detailSkills.length === 0 && !loadingSkillCards && (
                <div className="import-panel-v2__empty-hint">暫無技能規範</div>
              )}
              {detailSkills.map(skill => (
                <SkillCard
                  key={skill.skill_id}
                  skill={skill}
                  onToggle={async (id, active) => {
                    await api.updateAgentSkill(id, { is_active: active })
                    setDetailSkills(prev => prev.map(s =>
                      s.skill_id === id ? { ...s, is_active: active } : s
                    ))
                    window.dispatchEvent(new CustomEvent('skills-changed'))
                  }}
                  onDelete={async (id) => {
                    await api.deleteAgentSkill(id)
                    setDetailSkills(prev => prev.filter(s => s.skill_id !== id))
                    window.dispatchEvent(new CustomEvent('skills-changed'))
                  }}
                  onUpdate={async (id, title, trigger, behavior, injectionMode, agentScope) => {
                    await api.updateAgentSkill(id, { title, trigger, behavior, injection_mode: injectionMode, agent_scope: agentScope })
                    setDetailSkills(prev => prev.map(s =>
                      s.skill_id === id ? { ...s, title, trigger, behavior, injection_mode: injectionMode as 'passive' | 'active', agent_scope: agentScope } : s
                    ))
                    window.dispatchEvent(new CustomEvent('skills-changed'))
                  }}
                />
              ))}
            </div>
          </div>
        )}

        {/* 我的技能規範：全局管理頁面 */}
        {view.type === 'skills_hub' && (
          <SkillsHub />
        )}
      </div>
    </div>
  )
}

// ── Skills Hub 全局管理頁面 ────────────────────────────────────────────────────

export function SkillsHub() {
  const [skills, setSkills] = useState<AgentSkill[]>([])
  const [loading, setLoading] = useState(true)
  const [showCreate, setShowCreate] = useState(false)
  const [creating, setCreating] = useState(false)
  const [newTitle, setNewTitle] = useState('')
  const [newTrigger, setNewTrigger] = useState('')
  const [newBehavior, setNewBehavior] = useState('')
  const [newInjectionMode, setNewInjectionMode] = useState<'passive' | 'active'>('passive')
  const [newAgentScope, setNewAgentScope] = useState<AgentScope>('all')

  const loadSkills = useCallback(() => {
    invoke<AgentSkill[]>('list_agent_skills', { activeOnly: false })
      .then(s => setSkills(s))
      .catch(() => {})
      .finally(() => setLoading(false))
  }, [])

  useEffect(() => {
    loadSkills()
    window.addEventListener('skills-changed', loadSkills)
    return () => window.removeEventListener('skills-changed', loadSkills)
  }, [loadSkills])

  const activeCount = skills.filter(s => s.is_active).length
  const totalTriggers = skills.reduce((n, s) => n + s.trigger_count, 0)

  const handleCreate = async () => {
    if (!newTitle.trim() || !newTrigger.trim() || !newBehavior.trim()) return
    setCreating(true)
    try {
      const skill = await invoke<AgentSkill>('save_agent_skill', {
        knowledgeItemId: 'manual',
        title: newTitle.trim(),
        trigger: newTrigger.trim(),
        behavior: newBehavior.trim(),
        injectionMode: newInjectionMode,
        agentScope: newAgentScope,
      })
      setSkills(prev => [skill, ...prev])
      setShowCreate(false)
      setNewTitle(''); setNewTrigger(''); setNewBehavior(''); setNewInjectionMode('passive'); setNewAgentScope('all')
    } catch (e) {
      toast.error('建立失敗：' + fmtError(e))
    } finally {
      setCreating(false)
    }
  }

  const handleUpdate = async (id: string, title: string, trigger: string, behavior: string, injectionMode: string, agentScope: AgentScope) => {
    await api.updateAgentSkill(id, { title, trigger, behavior, injection_mode: injectionMode, agent_scope: agentScope })
    setSkills(prev => prev.map(s =>
      s.skill_id === id ? { ...s, title, trigger, behavior, injection_mode: injectionMode as 'passive' | 'active', agent_scope: agentScope } : s
    ))
  }

  return (
    <div className="import-panel-v2__skills-hub">
      <div className="import-panel-v2__skills-hub-header">
        <h3>我的技能規範</h3>
        <div className="import-panel-v2__skills-hub-stats">
          <span>{activeCount} 項啟用中</span>
          {totalTriggers > 0 && <span>共驅動 {totalTriggers} 次對話</span>}
        </div>
        <button className="import-panel-v2__skill-new-btn" onClick={() => setShowCreate(v => !v)}>
          <FontAwesomeIcon icon={faPlus} /> 新增技能
        </button>
      </div>

      {/* 新增技能表單 */}
      {showCreate && (
        <div className="import-panel-v2__skill-create-form">
          <input
            className="import-panel-v2__skill-edit-input"
            placeholder="技能標題"
            value={newTitle}
            onChange={e => setNewTitle(e.target.value)}
          />
          <label className="import-panel-v2__skill-edit-label">觸發條件</label>
          <textarea
            className="import-panel-v2__skill-edit-textarea"
            placeholder="當使用者問到...時"
            value={newTrigger}
            onChange={e => setNewTrigger(e.target.value)}
            rows={2}
          />
          <label className="import-panel-v2__skill-edit-label">行為規範</label>
          <textarea
            className="import-panel-v2__skill-edit-textarea"
            placeholder="應先...，再...，最後..."
            value={newBehavior}
            onChange={e => setNewBehavior(e.target.value)}
            rows={3}
          />
          <div className="import-panel-v2__skill-edit-tools" style={{ marginTop: 6 }}>
            <span className="import-panel-v2__skill-edit-label">觸發時機</span>
            {(['passive', 'active'] as const).map(mode => (
              <label key={mode} style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 11 }}>
                <input
                  type="radio"
                  name="new-injection-mode"
                  checked={newInjectionMode === mode}
                  onChange={() => setNewInjectionMode(mode)}
                />
                {mode === 'passive' ? '被動取用（相似度比對）' : '主動注入（每次對話）'}
              </label>
            ))}
          </div>
          <div className="import-panel-v2__skill-edit-tools" style={{ marginTop: 6 }}>
            <span className="import-panel-v2__skill-edit-label">適用範圍</span>
            {ALL_SCOPES.map(scope => (
              <label key={scope} style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 11 }}>
                <input
                  type="radio"
                  name="new-agent-scope"
                  checked={newAgentScope === scope}
                  onChange={() => setNewAgentScope(scope)}
                />
                <span style={{ color: SCOPE_COLORS[scope] }}>{SCOPE_LABELS[scope]}</span>
              </label>
            ))}
          </div>
          <div className="import-panel-v2__skill-edit-footer">
            <button className="import-panel-v2__skill-save-btn" onClick={handleCreate} disabled={creating || !newTitle.trim() || !newTrigger.trim() || !newBehavior.trim()}>
              {creating ? <FontAwesomeIcon icon={faSpinner} spin /> : <><FontAwesomeIcon icon={faCheck} /> 建立</>}
            </button>
            <button className="import-panel-v2__skill-cancel-btn" onClick={() => setShowCreate(false)}>取消</button>
          </div>
        </div>
      )}

      {/* 統計面板 */}
      {!loading && skills.length > 0 && <SkillStatsPanel />}

      {loading && <FontAwesomeIcon icon={faSpinner} spin />}
      {!loading && skills.length === 0 && !showCreate && (
        <div className="import-panel-v2__empty-hint">
          尚無技能規範 — 點「新增技能」手動建立，或開啟知識項目讓 AI 自動建議
        </div>
      )}
      <div className="import-panel-v2__skills-hub-list">
        {skills.map(skill => (
          <SkillCard
            key={skill.skill_id}
            skill={skill}
            onToggle={async (id, active) => {
              await api.updateAgentSkill(id, { is_active: active })
              setSkills(prev => prev.map(s => s.skill_id === id ? { ...s, is_active: active } : s))
            }}
            onDelete={async (id) => {
              await api.deleteAgentSkill(id)
              setSkills(prev => prev.filter(s => s.skill_id !== id))
            }}
            onUpdate={handleUpdate}
          />
        ))}
      </div>
    </div>
  )
}

// ── Chat Message Sub-component ────────────────────────────────────────────────

function ChatMessage({
  msg,
  onSave,
  saving,
  isSaved,
}: {
  msg: KBMessage
  onSave: (msg: KBMessage) => void
  saving: boolean
  isSaved?: boolean
}) {
  const isAssistant = msg.role === 'assistant'
  return (
    <div className={`import-panel-v2__msg ${isAssistant ? 'assistant' : 'user'}`}>
      <div className="import-panel-v2__msg-bubble">
        {msg.isStreaming && !msg.content ? (
          <span className="import-panel-v2__typing">
            <span /><span /><span />
          </span>
        ) : (
          <span style={{ whiteSpace: 'pre-wrap' }}>{msg.content}</span>
        )}
        {msg.error && <span className="import-panel-v2__msg-error"> ({msg.error})</span>}
      </div>

      {/* Source refs */}
      {isAssistant && msg.refs && msg.refs.length > 0 && (
        <div className="import-panel-v2__msg-refs">
          {msg.refs.map((r, i) => (
            <a key={i} href={r.path} target="_blank" rel="noreferrer" className="import-panel-v2__msg-ref">
              <FontAwesomeIcon icon={faExternalLinkAlt} /> {r.title}
            </a>
          ))}
        </div>
      )}

      {/* Save as knowledge button */}
      {isAssistant && !msg.isStreaming && msg.refs && msg.refs.length > 0 && (
        <button
          className={isSaved ? 'import-panel-v2__saved-btn' : 'import-panel-v2__save-btn'}
          onClick={() => { if (!isSaved) onSave(msg) }}
          disabled={saving || isSaved}
          title={isSaved ? '已儲存' : '儲存為知識'}
        >
          {isSaved
            ? <><FontAwesomeIcon icon={faCheck} /> 已儲存</>
            : saving
              ? <><FontAwesomeIcon icon={faSpinner} spin /> 儲存中…</>
              : <><FontAwesomeIcon icon={faBookmark} /> 儲存為知識</>}
        </button>
      )}
    </div>
  )
}

// ── SkillCard Sub-component ───────────────────────────────────────────────────

/** Extract ordered @[tool_name] markers from behavior text. */
function extractChainFromBehavior(behavior: string): string[] {
  const result: string[] = []
  const seen = new Set<string>()
  let rest = behavior
  while (true) {
    const start = rest.indexOf('@[')
    if (start === -1) break
    rest = rest.slice(start + 2)
    const end = rest.indexOf(']')
    if (end === -1) break
    const name = rest.slice(0, end).trim()
    if (name && !seen.has(name)) { seen.add(name); result.push(name) }
    rest = rest.slice(end + 1)
  }
  return result
}

function fmtLastTriggered(ts: number | null): string | null {
  if (!ts) return null
  // last_triggered_at is stored as Unix seconds by the service
  const days = Math.floor((Date.now() - ts * 1000) / 86400000)
  if (days <= 0) return '今天觸發'
  if (days === 1) return '昨天觸發'
  return `${days} 天前觸發`
}

const ALL_SCOPES: AgentScope[] = ['all', 'main', 'search', 'write', 'research', 'memory']
const SCOPE_LABELS: Record<AgentScope, string> = {
  all: '全體', main: '主 Agent', search: '搜尋', write: '寫入', research: '研究', memory: '記憶',
}
const SCOPE_COLORS: Record<AgentScope, string> = {
  all: 'var(--color-text-muted)',
  main: 'var(--color-accent)',
  search: '#22c55e',
  write: '#f59e0b',
  research: '#8b5cf6',
  memory: '#ec4899',
}

function SkillCard({
  skill,
  onToggle,
  onDelete,
  onUpdate,
}: {
  skill: AgentSkill
  onToggle: (id: string, active: boolean) => Promise<void>
  onDelete: (id: string) => Promise<void>
  onUpdate?: (id: string, title: string, trigger: string, behavior: string, injectionMode: string, agentScope: AgentScope) => Promise<void>
}) {
  const [toggling, setToggling] = useState(false)
  const [editing, setEditing] = useState(false)
  const [saving, setSaving] = useState(false)
  const [editTitle, setEditTitle] = useState(skill.title)
  const [editTrigger, setEditTrigger] = useState(skill.trigger)
  const [editBehavior, setEditBehavior] = useState(skill.behavior)
  const [editInjectionMode, setEditInjectionMode] = useState<'passive' | 'active'>(skill.injection_mode ?? 'passive')
  const [editAgentScope, setEditAgentScope] = useState<AgentScope>(skill.agent_scope ?? 'all')

  const daysSinceTrigger = skill.last_triggered_at
    ? Math.floor((Date.now() - skill.last_triggered_at * 1000) / 86400000)
    : null
  const isStale = skill.is_active && skill.trigger_count > 0 && daysSinceTrigger !== null && daysSinceTrigger > 90
  const neverTriggered = skill.trigger_count === 0

  const handleSaveEdit = async () => {
    if (!onUpdate) return
    setSaving(true)
    try {
      await onUpdate(skill.skill_id, editTitle, editTrigger, editBehavior, editInjectionMode, editAgentScope)
      setEditing(false)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className={`import-panel-v2__skill-card ${skill.is_active ? 'active' : 'inactive'}${isStale ? ' stale' : ''}`}>
      <div className="import-panel-v2__skill-header">
        {editing
          ? <input
              className="import-panel-v2__skill-edit-input"
              value={editTitle}
              onChange={e => setEditTitle(e.target.value)}
              placeholder="技能標題"
            />
          : <span className="import-panel-v2__skill-title">{skill.title}</span>
        }
        <div className="import-panel-v2__skill-actions">
          {skill.trigger_count > 0 && !editing && (
            <span className="import-panel-v2__skill-usage-badge" title={fmtLastTriggered(skill.last_triggered_at) ?? ''}>
              觸發 {skill.trigger_count} 次
            </span>
          )}
          {!editing && onUpdate && (
            <button
              className="import-panel-v2__skill-edit-btn"
              onClick={() => setEditing(true)}
              title="編輯技能"
            >
              <FontAwesomeIcon icon={faPen} />
            </button>
          )}
          {!editing && (
            <button
              className={`import-panel-v2__skill-toggle ${skill.is_active ? 'on' : 'off'}`}
              disabled={toggling}
              onClick={async () => {
                setToggling(true)
                await onToggle(skill.skill_id, !skill.is_active)
                setToggling(false)
              }}
            >
              {toggling ? <FontAwesomeIcon icon={faSpinner} spin /> : skill.is_active ? '啟用中' : '已停用'}
            </button>
          )}
          {!editing && (
            <button className="import-panel-v2__skill-delete" onClick={() => onDelete(skill.skill_id)} title="刪除技能">
              <FontAwesomeIcon icon={faTrash} />
            </button>
          )}
        </div>
      </div>

      {editing ? (
        <div className="import-panel-v2__skill-edit-body">
          <label className="import-panel-v2__skill-edit-label">觸發條件</label>
          <textarea
            className="import-panel-v2__skill-edit-textarea"
            value={editTrigger}
            onChange={e => setEditTrigger(e.target.value)}
            rows={2}
            placeholder="當使用者問到...時"
          />
          <label className="import-panel-v2__skill-edit-label">行為規範</label>
          <textarea
            className="import-panel-v2__skill-edit-textarea"
            value={editBehavior}
            onChange={e => setEditBehavior(e.target.value)}
            rows={3}
            placeholder="應先...，再...，最後..."
          />
          <div className="import-panel-v2__skill-edit-tools" style={{ marginTop: 6 }}>
            <span className="import-panel-v2__skill-edit-label">觸發時機</span>
            {(['passive', 'active'] as const).map(mode => (
              <label key={mode} style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 11 }}>
                <input
                  type="radio"
                  name={`injection-${skill.skill_id}`}
                  checked={editInjectionMode === mode}
                  onChange={() => setEditInjectionMode(mode)}
                />
                {mode === 'passive' ? '被動取用（相似度比對）' : '主動注入（每次對話）'}
              </label>
            ))}
          </div>
          <div className="import-panel-v2__skill-edit-tools" style={{ marginTop: 6 }}>
            <span className="import-panel-v2__skill-edit-label">適用範圍</span>
            {ALL_SCOPES.map(scope => (
              <label key={scope} style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 11 }}>
                <input
                  type="radio"
                  name={`scope-${skill.skill_id}`}
                  checked={editAgentScope === scope}
                  onChange={() => setEditAgentScope(scope)}
                />
                <span style={{ color: SCOPE_COLORS[scope] }}>{SCOPE_LABELS[scope]}</span>
              </label>
            ))}
          </div>
          <div className="import-panel-v2__skill-edit-footer">
            <button className="import-panel-v2__skill-save-btn" onClick={handleSaveEdit} disabled={saving}>
              {saving ? <FontAwesomeIcon icon={faSpinner} spin /> : <><FontAwesomeIcon icon={faCheck} /> 儲存</>}
            </button>
            <button className="import-panel-v2__skill-cancel-btn" onClick={() => {
              setEditing(false)
              setEditTitle(skill.title)
              setEditTrigger(skill.trigger)
              setEditBehavior(skill.behavior)
              setEditInjectionMode(skill.injection_mode ?? 'passive')
              setEditAgentScope(skill.agent_scope ?? 'all')
            }}>取消</button>
          </div>
        </div>
      ) : (
        <>
          <div className="import-panel-v2__skill-trigger"><strong>觸發：</strong>{skill.trigger}</div>
          <div className="import-panel-v2__skill-behavior"><strong>行為：</strong>{skill.behavior}</div>
          {(() => {
            const chain = extractChainFromBehavior(skill.behavior)
            return chain.length > 0 ? (
              <div style={{ marginTop: 4, display: 'flex', gap: 3, flexWrap: 'wrap' }}>
                {chain.map((t: string, i: number) => (
                  <span key={i} style={{
                    fontSize: 10, padding: '1px 6px', borderRadius: 8,
                    background: 'rgba(16,185,129,0.1)', color: 'var(--color-success)',
                    border: '1px solid var(--color-success)', fontFamily: 'monospace',
                  }}>@{t}</span>
                ))}
              </div>
            ) : null
          })()}
          <div style={{ marginTop: 4, display: 'flex', gap: 4, flexWrap: 'wrap' }}>
            <span style={{
              fontSize: 10, padding: '1px 6px', borderRadius: 8,
              background: skill.injection_mode === 'active' ? 'rgba(99,102,241,0.15)' : 'var(--color-bg-overlay)',
              color: skill.injection_mode === 'active' ? 'var(--color-accent)' : 'var(--color-text-muted)',
              border: `1px solid ${skill.injection_mode === 'active' ? 'var(--color-accent)' : 'var(--color-border)'}`,
            }}>
              {skill.injection_mode === 'active' ? '⚡ 主動注入' : '🔍 被動取用'}
            </span>
            {(skill.agent_scope ?? 'all') !== 'all' && (
              <span style={{
                fontSize: 10, padding: '1px 6px', borderRadius: 8,
                background: 'var(--color-bg-overlay)',
                color: SCOPE_COLORS[skill.agent_scope ?? 'all'],
                border: `1px solid ${SCOPE_COLORS[skill.agent_scope ?? 'all']}`,
              }}>
                {SCOPE_LABELS[skill.agent_scope ?? 'all']}
              </span>
            )}
          </div>
          {isStale && (
            <div className="import-panel-v2__skill-health-warn">
              此技能已 {daysSinceTrigger} 天未觸發，是否
              <button onClick={() => onToggle(skill.skill_id, false)}>停用</button>？
            </div>
          )}
          {neverTriggered && skill.is_active && (
            <div className="import-panel-v2__skill-never-used">尚未觸發 — 等待相關對話</div>
          )}
          {skill.last_triggered_at && (
            <div className="import-panel-v2__skill-last-used">{fmtLastTriggered(skill.last_triggered_at)}</div>
          )}
        </>
      )}
    </div>
  )
}
