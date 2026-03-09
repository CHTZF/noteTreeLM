import { useEffect, useRef, useState, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useSettingsStore } from '../../stores/settingsStore'
import { useVoiceRecorder } from '../../hooks/useVoiceRecorder'
import ConversationList from '../Chat/ConversationList'

// ── Types ─────────────────────────────────────────────────────────────────────

type LiveChatState = 'idle' | 'listening' | 'transcribing' | 'thinking' | 'speaking'

interface Message {
  role: 'user' | 'assistant'
  content: string
  wikilinks?: { label: string; absPath: string }[]
}

// ── Constants ─────────────────────────────────────────────────────────────────

const LIVE_CHAT_SYSTEM = `你是一個語音對話助手，協助使用者操作他們的筆記庫（Vault）。

## 最高優先規則（違反即為嚴重錯誤）
- 絕對禁止憑空捏造任何筆記名稱、資料夾名稱、或筆記內容——必須先呼叫工具取得真實資料才能回答
- 任何關於 Vault 結構、筆記內容的問題，一律先呼叫工具（list_structure / read_note / search_vault）
- 不得根據「猜測」或「上下文推測」回答 Vault 相關事實，必須以工具回傳結果為準

## 工具使用規則
- 使用者詢問資料夾結構、包含什麼 → 呼叫 list_structure（傳入資料夾路徑；根目錄傳空字串）
- 使用者說某個資料夾名稱（如「功能」「設定」）作為追問 → 呼叫 list_structure 查詢該資料夾路徑
- 使用者說「打開」「跳轉到」「看一下」「開啟」某筆記 → 呼叫 open_note，不要用 read_note
- 使用者說「念一下內容」「裡面寫什麼」→ 用 read_note 取得後，用 1-2 句口頭摘要
- 工具執行成功後，只說一句口頭確認，不複述工具原始輸出

## 回答格式（嚴格遵守）
- 只輸出自然、口語化的繁體中文，控制在 2-3 句話以內
- 絕對不能輸出 Markdown、列表符號（-/*/1.）、大括號、換行符號、程式碼`

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

// Confirmation keywords recognized as "yes, open that note" when note suggestions
// are pending. Bypasses the LLM entirely (local models hallucinate open_note calls).
const VOICE_CONFIRM_RE = /^(要|好|是|好的|對|確認|打開|開啟|開一下|看一下|看看|幫我打開|請打開|好啊|可以|沒問題|就這個|開它|打開它|要看|我要看|行|ok|OK|好喔|沒錯)$/

// ── Component ─────────────────────────────────────────────────────────────────

interface LiveChatPanelProps {
  onOpenNote: (path: string) => void
  onActiveChange?: (active: boolean) => void
}

export default function LiveChatPanel({ onOpenNote, onActiveChange }: LiveChatPanelProps) {
  const { settings } = useSettingsStore()

  // ── UI state ──────────────────────────────────────────────────────────────
  const [liveChatState, setLiveChatState] = useState<LiveChatState>('idle')
  const [messages, setMessages] = useState<Message[]>([])
  const [conversationId, setConversationId] = useState<string | null>(null)
  const [sidebarOpen, _setSidebarOpen] = useState(true)
  const [displayTranscript, setDisplayTranscript] = useState('')
  const [streamingText, setStreamingText] = useState('')
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

  // 初始化 conversation（live_chat mode）
  useEffect(() => {
    const saved = localStorage.getItem('live_chat_conversation_id')
    if (saved) {
      setConversationId(saved)
    } else {
      invoke<string>('create_conversation', { mode: 'live_chat' }).then(id => {
        setConversationId(id)
        localStorage.setItem('live_chat_conversation_id', id)
      }).catch(() => {})
    }
  }, [])

  const handleSelectConversation = useCallback((id: string) => {
    setConversationId(id)
    localStorage.setItem('live_chat_conversation_id', id)
    setMessages([])
  }, [])

  const handleNewConversation = useCallback((id: string) => {
    if (!id) return
    setConversationId(id)
    localStorage.setItem('live_chat_conversation_id', id)
    setMessages([])
  }, [])


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
    settings.whisper_language ?? 'auto',
  )

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

  // ── LLM streaming + chunked TTS ───────────────────────────────────────────
  const sendToLLM = useCallback(async (query: string) => {
    // ── Voice shortcut check ──────────────────────────────────────────────────
    // Must read ref before setNoteSuggestions([]) clears the state
    const prevSuggestions = noteSuggestionsRef.current
    if (prevSuggestions.length > 0 && VOICE_CONFIRM_RE.test(query.trim())) {
      const note = prevSuggestions[0]
      setNoteSuggestions([])
      noteSuggestionsRef.current = []
      onOpenNote(note.absPath)
      const confirmText = `好的，已為你打開${note.label}筆記`
      setMessages(prev => [...prev,
        { role: 'user', content: query },
        { role: 'assistant', content: confirmText },
      ])
      setStreamingText('')
      setLiveChatState('speaking')
      const utt = new SpeechSynthesisUtterance(confirmText)
      utt.lang = 'zh-TW'
      utt.rate = 1.15
      utt.onend = () => {
        setTimeout(() => {
          if (liveChatStateRef.current === 'speaking') {
            transcriptRef.current = ''
            setDisplayTranscript('')
            setLiveChatState('listening')
            toggle()
          }
        }, 200)
      }
      window.speechSynthesis.speak(utt)
      return  // skip LLM entirely
    }

    setLiveChatState('thinking')
    setStreamingText('')
    setNoteSuggestions([])   // clear previous note suggestions
    setMessages(prev => [...prev, { role: 'user', content: query }])

    // Local vars for this invocation — captured in closures below
    let ttsBuffer = ''
    const ttsQueue: string[] = []
    let ttsActive = false
    let llmDone = false
    let fullResponse = ''
    const localStreamingRef = { current: '' }

    function drainTTSQueue() {
      if (ttsActive || ttsQueue.length === 0) return
      ttsActive = true
      setLiveChatState('speaking')
      const utt = new SpeechSynthesisUtterance(ttsQueue.shift()!)
      utt.lang = 'zh-TW'
      utt.rate = 1.15
      utt.onend = () => {
        ttsActive = false
        if (ttsQueue.length > 0) {
          drainTTSQueue()
        } else if (llmDone) {
          // All TTS done & LLM done → commit message, go back to listening
          if (fullResponse) {
            setMessages(prev => [...prev, {
              role: 'assistant', content: fullResponse,
              wikilinks: localNoteRefs.length > 0 ? localNoteRefs : undefined,
            }])
          }
          setStreamingText('')
          // Use a small delay to let state settle before starting recorder again
          setTimeout(() => {
            if (liveChatStateRef.current === 'speaking') {
              transcriptRef.current = ''
              setDisplayTranscript('')
              setLiveChatState('listening')
              toggle()
            }
          }, 200)
        }
      }
      window.speechSynthesis.speak(utt)
    }

    function enqueueSentence(text: string) {
      const trimmed = text.trim()
      if (!trimmed) return
      ttsQueue.push(trimmed)
      drainTTSQueue()
    }

    function flushBuffer(force = false) {
      // Try to cut at a strong sentence-ending punctuation first
      const sentenceMatch = ttsBuffer.match(/^([\s\S]*?[。！？])(.*)$/)
      if (sentenceMatch) {
        enqueueSentence(sentenceMatch[1])
        ttsBuffer = sentenceMatch[2]
        // Recurse in case there are more complete sentences in the remainder
        if (ttsBuffer.includes('。') || ttsBuffer.includes('！') || ttsBuffer.includes('？')) {
          flushBuffer()
        }
        return
      }
      // Fall back to comma/pause punctuation if buffer is long enough
      if (ttsBuffer.length > 25) {
        const commaIdx = ttsBuffer.search(/[，、；]/)
        if (commaIdx >= 0) {
          enqueueSentence(ttsBuffer.slice(0, commaIdx + 1))
          ttsBuffer = ttsBuffer.slice(commaIdx + 1)
          return
        }
      }
      // Force flush remaining text (called at llm:done)
      if (force && ttsBuffer.trim()) {
        enqueueSentence(ttsBuffer)
        ttsBuffer = ''
      }
    }

    const unlistenToken = await listen<string>('llm:token', (e) => {
      fullResponse += e.payload
      localStreamingRef.current += e.payload
      setStreamingText(localStreamingRef.current)
      ttsBuffer += e.payload
      flushBuffer()
    })

    // Tool call in progress: discard any preamble text from TTS buffer
    // (e.g. "我幫你查詢" — LLM will reply again after tool execution),
    // and show a searching indicator instead.
    const unlistenToolCall = await listen<string>('agent:tool_call', () => {
      ttsBuffer = ''
      localStreamingRef.current = ''
      setStreamingText('搜尋中…')
    })

    // Write tool in voice mode: auto-approve (no UI confirmation dialog).
    const unlistenWriteReq = await listen<string>('agent:write_request', () => {
      invoke('confirm_write_tool', { approved: true }).catch(() => {})
    })

    // Note navigation: show status-bar buttons immediately and accumulate refs
    // so they can be embedded as wikilinks in the committed assistant message.
    let localNoteRefs: { label: string; absPath: string }[] = []
    const unlistenNoteRefs = await listen<string[]>('agent:note_refs', (e) => {
      const suggestions = e.payload.map(absPath => ({
        absPath,
        label: absPath.split('/').pop()?.replace(/\.md$/, '') ?? absPath,
      }))
      setNoteSuggestions(suggestions)
      // Deduplicate accumulation (multiple tool rounds may fire the same refs)
      for (const s of suggestions) {
        if (!localNoteRefs.some(r => r.absPath === s.absPath)) localNoteRefs.push(s)
      }
    })

    const unlistenDone = await listen('llm:done', () => {
      llmDone = true
      flushBuffer(true)
      unlistenToken()
      unlistenToolCall()
      unlistenWriteReq()
      unlistenNoteRefs()
      unlistenDone()
      // If TTS queue is empty and nothing is playing, transition now
      if (!ttsActive && ttsQueue.length === 0) {
        if (fullResponse) {
          setMessages(prev => [...prev, {
            role: 'assistant', content: fullResponse,
            wikilinks: localNoteRefs.length > 0 ? localNoteRefs : undefined,
          }])
        }
        setStreamingText('')
        setTimeout(() => {
          if (liveChatStateRef.current === 'thinking' || liveChatStateRef.current === 'speaking') {
            transcriptRef.current = ''
            setDisplayTranscript('')
            setLiveChatState('listening')
            toggle()
          }
        }, 200)
      }
    })

    try {
      const convId = conversationId
      await invoke('invoke_agent', {
        input: query,
        messages: [...messagesRef.current, { role: 'user', content: query }],
        system: LIVE_CHAT_SYSTEM,
        conversationId: convId ?? undefined,
      })
    } catch {
      unlistenToken()
      unlistenToolCall()
      unlistenWriteReq()
      unlistenNoteRefs()
      unlistenDone()
      window.speechSynthesis.cancel()
      setTimeout(() => {
        transcriptRef.current = ''
        setDisplayTranscript('')
        setLiveChatState('listening')
        toggle()
      }, 200)
    }
  }, [toggle])

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
    setMessages([])
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
              {msg.content}
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
            <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>找到筆記，要打開嗎？</span>
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
      </div>{/* end main live chat area */}
    </div>
  )
}
