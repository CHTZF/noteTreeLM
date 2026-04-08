import { useEffect, useRef, useState, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { useTranslation } from 'react-i18next'
import { listen } from '@tauri-apps/api/event'
import { api } from '../../lib/api'
import { useSettingsStore } from '../../stores/settingsStore'
import { useVoiceRecorder } from '../../hooks/useVoiceRecorder'
import { useDebugStore } from '../../stores/debugStore'
import { useLiveChatStore } from '../../stores/liveChatStore'
import ConversationList from '../Chat/ConversationList'

// ── Types ─────────────────────────────────────────────────────────────────────

type LiveChatState = 'idle' | 'listening' | 'transcribing' | 'thinking' | 'speaking'

interface Message {
  role: 'user' | 'assistant'
  content: string
  wikilinks?: { label: string; absPath: string }[]
}

// ── Constants ─────────────────────────────────────────────────────────────────

const STATE_LABEL: Record<LiveChatState, string> = {
  idle:         '已暫停',
  listening:    '聆聽中…',
  transcribing: '辨識中…',
  thinking:     '思考中…',
  speaking:     '回答中…',
}

const STATE_COLOR: Record<LiveChatState, string> = {
  idle:         'var(--color-border)',
  listening:    'var(--color-accent)',
  transcribing: 'var(--color-warning, #f59e0b)',
  thinking:     'var(--color-text-muted)',
  speaking:     'var(--color-success, #22c55e)',
}

// ── Vault image parsing ───────────────────────────────────────────────────────

const IMAGE_EXTS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif'])

type ContentPart =
  | { type: 'text'; text: string }
  | { type: 'image'; name: string; url: string }

function parseContentParts(content: string): ContentPart[] {
  const parts = content.split(/!\[\[([^\]]+)\]\]/g)
  const result: ContentPart[] = []
  parts.forEach((part, i) => {
    if (i % 2 === 0) {
      if (part) result.push({ type: 'text', text: part })
    } else {
      // Strip optional |name or |name|size suffix → actual path
      const actualPath = part.split('|')[0].trim()
      const ext = actualPath.split('.').pop()?.toLowerCase() ?? ''
      if (IMAGE_EXTS.has(ext)) {
        result.push({ type: 'image', name: actualPath, url: `vault://localhost/${actualPath}` })
      } else {
        result.push({ type: 'text', text: `![[${part}]]` })
      }
    }
  })
  return result
}

// ── Component ─────────────────────────────────────────────────────────────────

interface LiveChatPanelProps {
  onOpenNote: (path: string) => void
  onActiveChange?: (active: boolean) => void
}

export default function LiveChatPanel({ onOpenNote, onActiveChange }: LiveChatPanelProps) {
  const { settings } = useSettingsStore()
  const { i18n } = useTranslation()
  const addLog = useDebugStore(s => s.addLog)

  // ── UI state ──────────────────────────────────────────────────────────────
  const [liveChatState, setLiveChatState] = useState<LiveChatState>('idle')
  // messages persisted in store so they survive pane remounts
  const messages = useLiveChatStore(s => s.messages) as Message[]
  const setMessages = useLiveChatStore(s => s.setMessages) as (updater: (prev: Message[]) => Message[]) => void
  const [conversationId, setConversationId] = useState<string | null>(null)
  const [sidebarOpen, _setSidebarOpen] = useState(true)
  const [displayTranscript, setDisplayTranscript] = useState('')
  const [streamingText, setStreamingText] = useState('')
  const [thinkingText, setThinkingText] = useState('')
  // Note suggestions from search_vault / read_note tool calls
  const [noteSuggestions, setNoteSuggestions] = useState<{ absPath: string; label: string }[]>([])
  // Ref mirrors noteSuggestions so sendToLLM can read the value that was set
  // *before* setNoteSuggestions([]) runs at the top of sendToLLM
  const noteSuggestionsRef = useRef<{ absPath: string; label: string }[]>([])
  const messagesEndRef = useRef<HTMLDivElement>(null)

  // ── Internal refs (avoid stale closures in callbacks) ─────────────────────
  const transcriptRef = useRef('')
  const silenceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const liveChatStateRef = useRef<LiveChatState>('idle')
  const messagesRef = useRef<Message[]>([])
  // True once isSpeaking has fired at least once this listening round.
  // Prevents the silence timer from looping when no speech has occurred yet.
  const hasSpokenRef = useRef(false)

  // Keep refs in sync with state
  useEffect(() => { liveChatStateRef.current = liveChatState }, [liveChatState])
  useEffect(() => { messagesRef.current = messages }, [messages])
  useEffect(() => { noteSuggestionsRef.current = noteSuggestions }, [noteSuggestions])

  // Notify parent when active/idle changes (for Chat voice mutex)
  useEffect(() => {
    onActiveChange?.(liveChatState !== 'idle')
  }, [liveChatState, onActiveChange])

  // 載入對話訊息（從 DB 恢復）
  const loadConversationMessages = useCallback(async (id: string) => {
    console.log('[LiveChatPanel] loadConversationMessages id:', id)
    try {
      const snap = await api.getConversation(id) as { messages_json: string }
      console.log('[LiveChatPanel] get_conversation raw messages_json:', snap.messages_json)
      const msgs: Array<{ role: string; content: string }> = JSON.parse(snap.messages_json)
      const filtered = msgs.filter(m => m.role === 'user' || m.role === 'assistant')
      console.log('[LiveChatPanel] parsed', msgs.length, 'msgs, filtered to', filtered.length)
      setMessages(() => filtered.map(m => ({ role: m.role as 'user' | 'assistant', content: m.content })))
    } catch (e) {
      console.error('[LiveChatPanel] loadConversationMessages error:', e)
      setMessages(() => [])
    }
  }, [setMessages])

  // 初始化 conversation（live_chat mode）— 不自動建立新對話
  useEffect(() => {
    const saved = localStorage.getItem('live_chat_conversation_id')
    console.log('[LiveChatPanel] mount, saved conversation_id:', saved)
    if (saved) {
      api.getConversation(saved)
        .then(() => {
          console.log('[LiveChatPanel] verified conversation exists, loading messages for', saved)
          setConversationId(saved)
          loadConversationMessages(saved)
        })
        .catch((e) => {
          console.warn('[LiveChatPanel] stale conversation_id, clearing:', e)
          localStorage.removeItem('live_chat_conversation_id')
        })
    }
  }, [loadConversationMessages])

  const handleSelectConversation = useCallback((id: string) => {
    console.log('[LiveChatPanel] handleSelectConversation id:', id)
    setConversationId(id)
    localStorage.setItem('live_chat_conversation_id', id)
    loadConversationMessages(id)
  }, [loadConversationMessages])

  // Auto-scroll to bottom when messages or streaming changes
  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages, streamingText])

  // ── Voice recorder ────────────────────────────────────────────────────────
  const handleTranscript = useCallback((char: string) => {
    transcriptRef.current += char
    setDisplayTranscript(t => t + char)
  }, [])

  const { state: voiceState, isSpeaking, toggle } = useVoiceRecorder(
    handleTranscript,
    undefined,
    settings.voice_noise_suppression ?? true,
    5000,
    i18n.language,
  )

  const handleNewConversation = useCallback((id: string) => {
    if (!id) {
      // Current conversation was deleted — stop recording and go to empty screen
      window.speechSynthesis.cancel()
      if (silenceTimerRef.current) { clearTimeout(silenceTimerRef.current); silenceTimerRef.current = null }
      if (voiceState === 'recording') toggle()
      setLiveChatState('idle')
      setConversationId(null)
      localStorage.removeItem('live_chat_conversation_id')
      setMessages(() => [])
      setDisplayTranscript('')
      setStreamingText('')
      transcriptRef.current = ''
      return
    }
    setConversationId(id)
    localStorage.setItem('live_chat_conversation_id', id)
    setMessages(() => [])
  }, [voiceState, toggle])

  // ── startListening helper ─────────────────────────────────────────────────
  const startListening = useCallback(() => {
    transcriptRef.current = ''
    hasSpokenRef.current = false   // reset "has spoken" flag for the new round
    setDisplayTranscript('')
    setStreamingText('')
    setLiveChatState('listening')
    // Only call toggle() if not already recording
    // (useVoiceRecorder state: idle/done/error → start; recording → no-op needed)
    if (voiceState === 'idle' || voiceState === 'done' || voiceState === 'error') {
      toggle()
    }
  }, [voiceState, toggle])

  // ── Auto-stop: 1.5s silence → stop recording ─────────────────────────────
  useEffect(() => {
    if (liveChatState !== 'listening') return
    if (isSpeaking) {
      // User is speaking — mark it and clear any silence timer
      hasSpokenRef.current = true
      if (silenceTimerRef.current) {
        clearTimeout(silenceTimerRef.current)
        silenceTimerRef.current = null
      }
    } else {
      // User stopped speaking — only arm timer if they actually spoke this round.
      // This prevents the empty-transcript loop: without the hasSpokenRef guard,
      // the timer would fire every 1.5s when sitting silently, causing
      // 聆聽中 ↔ 辨識中 to alternate endlessly.
      // For short utterances: isSpeaking went true (sets hasSpokenRef=true) then false,
      // so the timer is armed even before whisper returns the transcript.
      if (voiceState === 'recording' && hasSpokenRef.current) {
        if (!silenceTimerRef.current) {
          silenceTimerRef.current = setTimeout(() => {
            silenceTimerRef.current = null
            toggle()                          // recording → transcribing
            setLiveChatState('transcribing')
          }, 1500)
        }
      }
    }
  }, [isSpeaking, liveChatState, voiceState, toggle])

  // Clean up silence timer on unmount
  useEffect(() => {
    return () => {
      if (silenceTimerRef.current) clearTimeout(silenceTimerRef.current)
    }
  }, [])

  // ── Wait for voiceState === 'done' or 'idle' → send to LLM ─────────────────
  // 'done' : queue 有剩餘 chunk，stopRecording 等待處理完後設 done
  // 'idle' : queue 在 toggle() 前已空（快機器/早 flush），stopRecording 直接設 idle
  //          兩條路 stopRecording 都等 typewriter 清空才設，所以 transcriptRef 此時完整
  useEffect(() => {
    if (liveChatState !== 'transcribing') return
    if (voiceState === 'done' || voiceState === 'idle') {
      const query = transcriptRef.current.trim()
      transcriptRef.current = ''
      setDisplayTranscript('')
      if (query) {
        sendToLLM(query)
      } else {
        startListening()
      }
    }
  }, [voiceState, liveChatState])  // sendToLLM/startListening intentionally omitted (defined below)

  // ── LLM invoke + TTS ─────────────────────────────────────────────────────
  const sendToLLM = useCallback(async (query: string) => {
    setLiveChatState('thinking')
    setStreamingText('')
    setThinkingText('')
    setNoteSuggestions([])
    setMessages(prev => [...prev, { role: 'user', content: query }])

    // Listen for agent:think (tool-phase inner monologue)
    const unlistenThink = await listen<{ thought: string }>('agent:think', (e) => {
      setThinkingText(e.payload.thought)
    })
    // Write tool in voice mode: auto-approve
    const unlistenWriteReq = await listen<string>('agent:write_request', () => {
      invoke('confirm_write_tool', { approved: true }).catch(() => {})
    })
    // Write confirmation timeout — voice mode auto-approves so this is rare,
    // but still needs cleanup to avoid stale state.
    const unlistenWriteTimeout = await listen<{ display: string }>('agent:write_timeout', () => {
      setThinkingText('')
    })
    const unlistenPlanAnnounce = await listen<{ session_id: string; plan: string }>('agent:plan_announce', (e) => {
      setThinkingText(e.payload.plan)
    })
    // Note refs
    let localNoteRefs: { label: string; absPath: string }[] = []
    const unlistenNoteRefs = await listen<{ paths: string[] }>('agent:note_refs', (e) => {
      const suggestions = e.payload.paths.map(absPath => {
        const hashIdx = absPath.indexOf('#')
        const filePart = hashIdx >= 0 ? absPath.slice(0, hashIdx) : absPath
        const section = hashIdx >= 0 ? absPath.slice(hashIdx + 1) : ''
        const filename = filePart.split('/').pop()?.replace(/\.md$/, '') ?? filePart
        const label = section ? `${filename} § ${section}` : filename
        return { absPath, label }
      })
      setNoteSuggestions(suggestions)
      for (const s of suggestions) {
        if (!localNoteRefs.some(r => r.absPath === s.absPath)) localNoteRefs.push(s)
      }
    })
    const unlistenOpenNote = await listen<string[]>('agent:open_note', (e) => {
      if (e.payload.length > 0) onOpenNote(e.payload[0])
    })

    const cleanup = () => {
      unlistenThink(); unlistenWriteReq(); unlistenWriteTimeout(); unlistenPlanAnnounce(); unlistenNoteRefs(); unlistenOpenNote()
      setThinkingText('')
    }

    try {
      const convId = conversationId
      // invoke_live_chat: blocking, returns final speech string from live_respond
      const speech = await invoke<string>('invoke_live_chat', {
        conversationId: convId ?? '',
        input: query,
        noteContext: null,
        activityContext: null,
        language: i18n.language,
        uiLanguage: i18n.language,
      })
      cleanup()

      if (!convId) {
        // New conversation: persist id from service (emitted via live_chat:action or read from resp)
        // For now rely on localStorage being set by the next round's conversationId detection
      }

      const speech_text = speech.trim()
      if (!speech_text) {
        transcriptRef.current = ''
        setDisplayTranscript('')
        setLiveChatState('listening')
        toggle()
        return
      }

      setMessages(prev => [...prev, {
        role: 'assistant', content: speech_text,
        wikilinks: localNoteRefs.length > 0 ? localNoteRefs : undefined,
      }])

      setLiveChatState('speaking')
      const utt = new SpeechSynthesisUtterance(speech_text)
      utt.lang = i18n.language
      utt.rate = 1.15
      utt.onend = () => {
        if (liveChatStateRef.current === 'speaking') {
          transcriptRef.current = ''
          setDisplayTranscript('')
          setLiveChatState('listening')
          toggle()
        }
      }
      window.speechSynthesis.speak(utt)

    } catch (e: unknown) {
      cleanup()
      const msg = typeof e === 'string' ? e : (e instanceof Error ? e.message : JSON.stringify(e))
      addLog('live-chat', 'error', `invoke_live_chat 失敗：${msg}`)
      window.speechSynthesis.cancel()
      setTimeout(() => {
        transcriptRef.current = ''
        setDisplayTranscript('')
        setLiveChatState('listening')
        toggle()
      }, 200)
    }
  }, [toggle, addLog, conversationId, i18n.language, onOpenNote])

  // ── Barge-in: user speaks while AI is speaking → cancel TTS + LLM stream ──
  useEffect(() => {
    if (liveChatState !== 'speaking') return
    if (isSpeaking) {
      window.speechSynthesis.cancel()
      invoke('cancel_agent').catch(() => {})  // signal Rust to stop SSE loop
      // VAD is already recording (useVoiceRecorder keeps running)
      // Just update state — the transcript will accumulate and auto-stop after 1.5s silence
      transcriptRef.current = ''
      setDisplayTranscript('')
      setStreamingText('')
      setLiveChatState('listening')
    }
  }, [isSpeaking, liveChatState])


  // ── Handlers ─────────────────────────────────────────────────────────────
  const handleTogglePause = useCallback(() => {
    if (liveChatState === 'idle') {
      startListening()
    } else {
      window.speechSynthesis.cancel()
      if (silenceTimerRef.current) { clearTimeout(silenceTimerRef.current); silenceTimerRef.current = null }
      if (voiceState === 'recording') toggle()
      setLiveChatState('idle')
    }
  }, [liveChatState, voiceState, toggle, startListening])

  const handleClear = useCallback(() => {
    setMessages(() => [])
    messagesRef.current = []
  }, [])

  // ── Pulse animation ───────────────────────────────────────────────────────
  const isPulsing = liveChatState === 'listening' || liveChatState === 'speaking'

  // ── Render ────────────────────────────────────────────────────────────────
  return (
    <div style={{
      display: 'flex', flexDirection: 'row', height: '100%',
      background: 'var(--color-bg-base)', color: 'var(--color-text-primary)',
    }}>
      {/* Conversation sidebar */}
      {sidebarOpen && (
        <div style={{
          width: '180px', flexShrink: 0, borderRight: '1px solid var(--color-border)',
          display: 'flex', flexDirection: 'column', overflow: 'hidden',
        }}>
          <ConversationList
            mode="live_chat"
            selectedId={conversationId}
            onSelect={handleSelectConversation}
            onNew={handleNewConversation}
          />
        </div>
      )}

      {/* Main live chat area */}
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden', minWidth: 0 }}>
      {/* No conversation selected — placeholder */}
      {!conversationId ? (
        <div style={{
          flex: 1, display: 'flex', flexDirection: 'column', alignItems: 'center',
          justifyContent: 'center', color: 'var(--color-text-muted)', fontSize: '13px',
          textAlign: 'center', lineHeight: 1.8, padding: '24px', gap: '12px',
        }}>
          <div>請選擇過去對話</div>
          <div style={{ color: 'var(--color-text-muted)', fontSize: '11px' }}>或點擊側欄「＋ 新對話」開始</div>
        </div>
      ) : <>
      {/* Header */}
      <div style={{
        display: 'flex', alignItems: 'center', justifyContent: 'space-between',
        padding: '8px 12px', borderBottom: '1px solid var(--color-border)',
        flexShrink: 0, height: '40px',
      }}>
        <span style={{ fontSize: '11px', fontWeight: 600, letterSpacing: '0.08em', color: 'var(--color-text-muted)' }}>
          LIVE CHAT
        </span>
        <button
          onClick={handleClear}
          title="清除對話"
          style={{ fontSize: '12px', color: 'var(--color-text-muted)', padding: '2px 6px', borderRadius: '4px' }}
          onMouseEnter={e => (e.currentTarget.style.color = 'var(--color-text-primary)')}
          onMouseLeave={e => (e.currentTarget.style.color = 'var(--color-text-muted)')}
        >
          清除
        </button>
      </div>

      {/* Messages */}
      <div style={{
        flex: 1, overflowY: 'auto', padding: '12px',
        display: 'flex', flexDirection: 'column', gap: '10px',
      }}>
        {messages.length === 0 && liveChatState === 'idle' && (
          <div style={{
            flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center',
            color: 'var(--color-text-muted)', fontSize: '13px', textAlign: 'center',
            lineHeight: 1.6, padding: '24px',
          }}>
            點擊「開始」或切換至此 tab 即可自動開始語音對話
          </div>
        )}
        {messages.map((msg, i) => (
          <div key={i} style={{
            display: 'flex', justifyContent: msg.role === 'user' ? 'flex-end' : 'flex-start',
          }}>
            <div style={{
              maxWidth: '85%', padding: '8px 12px', borderRadius: '12px',
              fontSize: '13px', lineHeight: 1.6, wordBreak: 'break-word',
              background: msg.role === 'user'
                ? 'var(--color-accent)'
                : 'var(--color-bg-secondary, var(--color-bg-hover))',
              color: msg.role === 'user'
                ? 'white'
                : 'var(--color-text-primary)',
              borderBottomRightRadius: msg.role === 'user' ? '4px' : '12px',
              borderBottomLeftRadius: msg.role === 'assistant' ? '4px' : '12px',
            }}>
              {parseContentParts(msg.content).map((part, idx) =>
                part.type === 'text'
                  ? <span key={idx}>{part.text}</span>
                  : <img key={idx} src={part.url} alt={part.name}
                      style={{ maxWidth: '100%', borderRadius: '6px', marginTop: '6px', display: 'block' }}
                      onError={(e) => { (e.currentTarget as HTMLImageElement).style.display = 'none' }}
                    />
              )}
              {msg.wikilinks && msg.wikilinks.length > 0 && (
                <div style={{ marginTop: '6px', display: 'flex', flexWrap: 'wrap', gap: '4px' }}>
                  {msg.wikilinks.map((wl, j) => (
                    <button
                      key={j}
                      onClick={() => onOpenNote(wl.absPath)}
                      title={wl.absPath}
                      style={{
                        padding: '1px 6px', borderRadius: '4px', fontSize: '11px',
                        fontFamily: 'monospace',
                        background: 'transparent',
                        border: '1px solid var(--color-accent)',
                        color: 'var(--color-accent)',
                        cursor: 'pointer',
                        lineHeight: 1.6,
                      }}
                      onMouseEnter={e => { e.currentTarget.style.background = 'var(--color-accent)'; e.currentTarget.style.color = 'white' }}
                      onMouseLeave={e => { e.currentTarget.style.background = 'transparent'; e.currentTarget.style.color = 'var(--color-accent)' }}
                    >
                      [[{wl.label}]]
                    </button>
                  ))}
                </div>
              )}
            </div>
          </div>
        ))}
        {/* Inner monologue from think tool */}
        {thinkingText && (
          <div style={{ display: 'flex', justifyContent: 'flex-start', paddingLeft: '4px' }}>
            <div style={{
              maxWidth: '80%', padding: '4px 10px', borderRadius: '8px',
              fontSize: '11px', lineHeight: 1.5, wordBreak: 'break-word',
              color: 'var(--color-text-muted)', fontStyle: 'italic',
              borderLeft: '2px solid var(--color-border)',
              opacity: 0.75,
            }}>
              {thinkingText}
              <span style={{ opacity: 0.4, marginLeft: '2px', animation: 'blink 1.2s step-end infinite' }}>…</span>
            </div>
          </div>
        )}
        {/* Streaming text (thinking → speaking transition) */}
        {streamingText && (
          <div style={{ display: 'flex', justifyContent: 'flex-start' }}>
            <div style={{
              maxWidth: '85%', padding: '8px 12px', borderRadius: '12px',
              borderBottomLeftRadius: '4px',
              fontSize: '13px', lineHeight: 1.6, wordBreak: 'break-word',
              background: 'var(--color-bg-secondary, var(--color-bg-hover))',
              color: 'var(--color-text-primary)', opacity: 0.8,
            }}>
              {streamingText}
              <span style={{ opacity: 0.5, animation: 'blink 1s step-end infinite' }}>▌</span>
            </div>
          </div>
        )}
        <div ref={messagesEndRef} />
      </div>

      {/* Status bar */}
      <div style={{
        flexShrink: 0, borderTop: '1px solid var(--color-border)',
        padding: '12px', display: 'flex', flexDirection: 'column', gap: '8px',
      }}>
        {/* Status indicator */}
        <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
          <div style={{
            width: '10px', height: '10px', borderRadius: '50%',
            background: STATE_COLOR[liveChatState],
            flexShrink: 0,
            boxShadow: isPulsing ? `0 0 0 0 ${STATE_COLOR[liveChatState]}` : 'none',
            animation: isPulsing ? 'live-chat-pulse 1.5s ease-out infinite' : 'none',
          }} />
          <span style={{ fontSize: '12px', color: 'var(--color-text-muted)' }}>
            {STATE_LABEL[liveChatState]}
          </span>
          {/* Volume bars when listening */}
          {liveChatState === 'listening' && isSpeaking && (
            <div style={{ display: 'flex', alignItems: 'center', gap: '2px', marginLeft: '4px' }}>
              {[1, 2, 3, 4].map(n => (
                <div key={n} style={{
                  width: '3px', borderRadius: '2px',
                  background: 'var(--color-accent)',
                  height: `${6 + n * 3}px`,
                  opacity: 0.7,
                  animation: `live-chat-bar${n} 0.4s ease-in-out infinite alternate`,
                }} />
              ))}
            </div>
          )}
        </div>

        {/* Note navigation suggestions */}
        {noteSuggestions.length > 0 && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: '4px' }}>
            <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>找到相關段落，要打開嗎？</span>
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: '4px' }}>
              {noteSuggestions.map((note, i) => (
                <button
                  key={i}
                  onClick={() => { onOpenNote(note.absPath); setNoteSuggestions([]) }}
                  style={{
                    padding: '3px 8px', borderRadius: '5px', fontSize: '12px',
                    background: 'var(--color-bg-hover)',
                    color: 'var(--color-text-primary)',
                    border: '1px solid var(--color-border)',
                    cursor: 'pointer', maxWidth: '100%',
                    overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
                  }}
                  onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-accent)', e.currentTarget.style.color = 'white', e.currentTarget.style.borderColor = 'var(--color-accent)')}
                  onMouseLeave={e => (e.currentTarget.style.background = 'var(--color-bg-hover)', e.currentTarget.style.color = 'var(--color-text-primary)', e.currentTarget.style.borderColor = 'var(--color-border)')}
                  title={note.absPath}
                >
                  📄 {note.label}
                </button>
              ))}
              <button
                onClick={() => setNoteSuggestions([])}
                style={{
                  padding: '3px 8px', borderRadius: '5px', fontSize: '12px',
                  color: 'var(--color-text-muted)', cursor: 'pointer',
                }}
                onMouseEnter={e => (e.currentTarget.style.color = 'var(--color-text-primary)')}
                onMouseLeave={e => (e.currentTarget.style.color = 'var(--color-text-muted)')}
              >
                不用
              </button>
            </div>
          </div>
        )}

        {/* Real-time transcript preview */}
        {displayTranscript && (
          <div style={{
            fontSize: '12px', color: 'var(--color-text-secondary)',
            background: 'var(--color-bg-hover)',
            padding: '6px 8px', borderRadius: '6px',
            lineHeight: 1.5, wordBreak: 'break-word',
            maxHeight: '60px', overflowY: 'auto',
          }}>
            {displayTranscript}
          </div>
        )}

        {/* Pause / Resume button */}
        <button
          onClick={handleTogglePause}
          style={{
            padding: '6px 12px', borderRadius: '6px', fontSize: '12px',
            background: liveChatState === 'idle'
              ? 'var(--color-accent)'
              : 'var(--color-bg-hover)',
            color: liveChatState === 'idle'
              ? 'white'
              : 'var(--color-text-secondary)',
            cursor: 'pointer',
            transition: 'all 0.15s',
          }}
          onMouseEnter={e => (e.currentTarget.style.opacity = '0.8')}
          onMouseLeave={e => (e.currentTarget.style.opacity = '1')}
        >
          {liveChatState === 'idle' ? '▶ 開始' : '⏸ 暫停'}
        </button>
      </div>

      {/* Inline keyframes via style tag */}
      <style>{`
        @keyframes live-chat-pulse {
          0%   { box-shadow: 0 0 0 0 currentColor; opacity: 1; }
          70%  { box-shadow: 0 0 0 6px transparent; opacity: 0.7; }
          100% { box-shadow: 0 0 0 0 transparent; opacity: 1; }
        }
        @keyframes blink {
          0%, 100% { opacity: 1; }
          50%       { opacity: 0; }
        }
        @keyframes live-chat-bar1 { from { height: 4px; } to { height: 10px; } }
        @keyframes live-chat-bar2 { from { height: 6px; } to { height: 14px; } }
        @keyframes live-chat-bar3 { from { height: 8px; } to { height: 12px; } }
        @keyframes live-chat-bar4 { from { height: 4px; } to { height: 16px; } }
      `}</style>
      </>}
      </div>{/* end main live chat area */}
    </div>
  )
}
