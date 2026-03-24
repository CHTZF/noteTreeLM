import { useEffect, useRef, useState, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { api } from '../../lib/api'
import { useSettingsStore } from '../../stores/settingsStore'
import { useVoiceRecorder } from '../../hooks/useVoiceRecorder'
import { useEditorStore } from '../../stores/editorStore'
import { useActivityStore } from '../../stores/activityStore'
import { useTranslation } from 'react-i18next'

// ── Types ────────────────────────────────────────────────────────────────────

type SheetState = 'idle' | 'listening' | 'processing' | 'speaking'

interface LiveChatAction {
  speech: string
  action: 'none' | 'show_results' | 'open_note' | 'open_tab' | 'show_error'
  content?: string
  paths?: string[]
  path?: string
  tab?: string
  error?: string
}

interface LiveChatSheetProps {
  open: boolean
  onClose: () => void
  onOpenNote: (path: string) => void
  onOpenTab: (tab: string) => void
  onShowResults: (paths: string[]) => void
}

// ── Component ────────────────────────────────────────────────────────────────

export default function LiveChatSheet({ open, onClose, onOpenNote, onOpenTab, onShowResults }: LiveChatSheetProps) {
  const { settings } = useSettingsStore()
  const { t } = useTranslation()
  const currentNotePath = useEditorStore(s => s.currentPath)

  // Read activePattern from store directly — no usePatternDetector here to avoid
  // background Tauri invocations that compete with invoke_live_chat on llama-server.
  const activePattern = useActivityStore(s => s.activePattern)
  const clearPredictiveMode = useActivityStore(s => s.clearPredictiveMode)

  const [sheetState, setSheetState] = useState<SheetState>('idle')
  const [conversationId, setConversationId] = useState<string | null>(null)
  const [displayTranscript, setDisplayTranscript] = useState('')
  const [processingHint, setProcessingHint] = useState('')
  const [errorCard, setErrorCard] = useState<string | null>(null)
  const [resultPaths, setResultPaths] = useState<string[]>([])
  const [resultContent, setResultContent] = useState<string | null>(null)
  const [resultExpanded, setResultExpanded] = useState(true)
  const [textInput, setTextInput] = useState('')

  const sheetStateRef = useRef<SheetState>('idle')
  const transcriptRef = useRef('')
  const silenceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const bargeInTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const hasSpokenRef = useRef(false)
  const convIdRef = useRef<string | null>(null)
  // true when sheetState was set to 'processing' via voice path (silence timer or auto-sync)
  // so done-effect knows to also accept voiceState='idle' (recorder finished before timer)
  const awaitingVoiceRef = useRef(false)

  useEffect(() => { sheetStateRef.current = sheetState }, [sheetState])
  useEffect(() => { convIdRef.current = conversationId }, [conversationId])

  // ── Init conversation ────────────────────────────────────────────────────
  useEffect(() => {
    api.getOrCreateLiveChatConv('__live_chat__')
      .then(result => setConversationId(result.id))
      .catch(console.error)
  }, [])

  // ── Voice recorder ───────────────────────────────────────────────────────
  const handleTranscript = useCallback((text: string) => {
    transcriptRef.current += text
    setDisplayTranscript(t => t + text)
  }, [])

  const { state: voiceState, isSpeaking, toggle } = useVoiceRecorder(
    handleTranscript,
    undefined,
    settings.voice_noise_suppression ?? true,
    5000,
    settings.whisper_language ?? 'auto',
  )

  const voiceStateRef = useRef(voiceState)
  voiceStateRef.current = voiceState

  // ── startListening ───────────────────────────────────────────────────────
  const startListening = useCallback(() => {
    transcriptRef.current = ''
    hasSpokenRef.current = false
    setDisplayTranscript('')
    setProcessingHint('')
    setErrorCard(null)
    setSheetState('listening')
    if (voiceStateRef.current !== 'recording') toggle()
  }, [toggle])

  // ── speakWithFallback ────────────────────────────────────────────────────
  // Web Speech onend sometimes never fires in Tauri webview; use a timeout as safety net.
  const speakWithFallback = useCallback((text: string) => {
    const utt = new SpeechSynthesisUtterance(text)
    utt.lang = settings.whisper_language ?? 'zh-TW'
    utt.rate = 1.1
    // Estimate: ~150ms per char, min 3s, max 20s
    const fallbackMs = Math.min(Math.max(text.length * 150, 3000), 20000)
    let resolved = false
    const timer = setTimeout(() => {
      if (!resolved) {
        resolved = true
        if (sheetStateRef.current !== 'idle') startListening()
      }
    }, fallbackMs)
    const done = () => {
      if (!resolved) {
        resolved = true
        clearTimeout(timer)
        if (sheetStateRef.current !== 'idle') startListening()
      }
    }
    utt.onend = done
    utt.onerror = (e) => {
      console.warn('[LiveChat] TTS onerror:', e.error)
      done()
    }
    window.speechSynthesis.speak(utt)
  }, [settings.whisper_language, startListening])

  // ── sendToLLM ────────────────────────────────────────────────────────────
  const sendToLLM = useCallback(async (query: string) => {
    const convId = convIdRef.current
    if (!convId) return

    setSheetState('processing')
    setProcessingHint('')
    setResultContent(null)
    setResultPaths([])
    setResultExpanded(true)
    setErrorCard(null)

    // Positive feedback: user spoke while pattern was active → Bayesian update (fire-and-forget)
    const snap = useActivityStore.getState()
    if (snap.activePattern) {
      const { signature } = snap.activePattern
      const vaultId = (await import('../../stores/settingsStore'))
        .useSettingsStore.getState().settings.system_current_vault_path ?? ''
      if (vaultId) {
        api.updatePatternScore(vaultId, signature, true).catch(() => {})
      }
      clearPredictiveMode()
    }

    const noteCtx = currentNotePath
      ? `當前開啟的筆記路徑：${currentNotePath}`
      : undefined

    let unlistenTool: (() => void) | null = null
    let unlistenAction: (() => void) | null = null
    // Prevents fallback TTS from firing when live_chat:action already handled speech
    let actionHandled = false

    try {
      unlistenTool = await listen<{ display: string }>('live_chat:tool_call', (e) => {
        setProcessingHint(e.payload.display)
      })

      unlistenAction = await listen<LiveChatAction>('live_chat:action', (e) => {
        const action = e.payload
        setProcessingHint('')

        if (action.speech) {
          actionHandled = true
          setSheetState('speaking')
          speakWithFallback(action.speech)
        }

        if (action.action === 'open_note' && action.path) {
          onOpenNote(action.path)
        } else if (action.action === 'open_tab' && action.tab) {
          onOpenTab(action.tab)
        } else if (action.action === 'show_results') {
          if (action.content) {
            setResultContent(action.content)
            setResultExpanded(true)
          }
          if (action.paths?.length) {
            setResultPaths(action.paths)
            onShowResults(action.paths)
          }
        } else if (action.action === 'show_error' && action.error) {
          setErrorCard(action.error)
        } else if (action.action === 'none' && !action.speech) {
          startListening()
        }
      })

      const TIMEOUT_MS = 60_000
      const speech = await Promise.race([
        invoke<string>('invoke_live_chat', {
          conversationId: convId,
          input: query,
          noteContext: noteCtx,
          activityContext: snap.getContextString() || null,
          language: settings.whisper_language ?? 'zh-TW',
        }),
        new Promise<never>((_, reject) =>
          setTimeout(() => reject(new Error('live_chat timeout')), TIMEOUT_MS)
        ),
      ])
      // Fallback: only speak if live_chat:action did NOT already handle it
      if (!actionHandled) {
        if (speech && sheetStateRef.current === 'processing') {
          setSheetState('speaking')
          setProcessingHint('')
          speakWithFallback(speech)
        } else if (!speech && sheetStateRef.current === 'processing') {
          startListening()
        }
      }
    } catch (e) {
      console.error('[LiveChatSheet] invoke_live_chat error:', e)
      setProcessingHint('')
      // Rust 已 emit 錯誤 action（若來得及）；前端做最後保險：說一句話 + 顯示錯誤
      if (sheetStateRef.current !== 'idle') {
        const isTimeout = e instanceof Error && e.message === 'live_chat timeout'
        const errMsg = isTimeout ? '請求逾時，請稍後再試。' : '抱歉，遇到了一些問題，請稍後再試。'
        setErrorCard(errMsg)
        setSheetState('speaking')
        speakWithFallback(errMsg)
      }
    } finally {
      unlistenTool?.()
      unlistenAction?.()
    }
  }, [currentNotePath, settings.whisper_language, startListening, speakWithFallback, onOpenNote, onOpenTab, onShowResults, clearPredictiveMode])

  // ── Silence detection → send to LLM ─────────────────────────────────────
  useEffect(() => {
    if (sheetState !== 'listening') return
    if (isSpeaking) {
      hasSpokenRef.current = true
      if (silenceTimerRef.current) { clearTimeout(silenceTimerRef.current); silenceTimerRef.current = null }
    } else if (hasSpokenRef.current && voiceState === 'recording') {
      silenceTimerRef.current = setTimeout(() => {
        awaitingVoiceRef.current = true
        if (voiceStateRef.current === 'recording') toggle()
        setSheetState('processing')
      }, 1500)
    }
    return () => {
      if (silenceTimerRef.current) {
        clearTimeout(silenceTimerRef.current)
        silenceTimerRef.current = null
      }
    }
  }, [isSpeaking, sheetState, voiceState, toggle])

  // ── Recorder auto-stopped while listening → sync sheetState to processing ──
  // useVoiceRecorder has its own internal silence timeout (5000ms). When it fires,
  // voiceState goes 'recording' → 'transcribing' before our 1500ms timer fires,
  // leaving sheetState stuck in 'listening'. This effect corrects that.
  useEffect(() => {
    if (voiceState === 'transcribing' && sheetState === 'listening') {
      if (silenceTimerRef.current) { clearTimeout(silenceTimerRef.current); silenceTimerRef.current = null }
      awaitingVoiceRef.current = true
      setSheetState('processing')
    }
  }, [voiceState, sheetState])

  // ── Wait for transcription done → send ───────────────────────────────────
  useEffect(() => {
    if (sheetState !== 'processing') return
    // Accept 'done' (normal path) or 'idle' (recorder finished before silence timer fired)
    // Only process when awaitingVoiceRef is true (came from voice path, not text input)
    if ((voiceState === 'done' || (voiceState === 'idle' && awaitingVoiceRef.current))) {
      awaitingVoiceRef.current = false
      const query = transcriptRef.current.trim()
      transcriptRef.current = ''
      setDisplayTranscript('')
      if (query) sendToLLM(query)
      else startListening()
    }
  }, [voiceState, sheetState, sendToLLM, startListening])

  // ── Barge-in: user speaks while TTS is playing ────────────────────────────
  // Debounce 500ms：TTS 回音通常是短暫毛刺（< 200ms），真實打斷會持續更久
  useEffect(() => {
    if (sheetState !== 'speaking') return
    if (isSpeaking) {
      bargeInTimerRef.current = setTimeout(() => {
        if (sheetStateRef.current === 'speaking') {
          window.speechSynthesis.cancel()
          setSheetState('listening')
        }
      }, 500)
    } else {
      if (bargeInTimerRef.current) { clearTimeout(bargeInTimerRef.current); bargeInTimerRef.current = null }
    }
    return () => {
      if (bargeInTimerRef.current) { clearTimeout(bargeInTimerRef.current); bargeInTimerRef.current = null }
    }
  }, [isSpeaking, sheetState])

  // ── Open / close lifecycle ───────────────────────────────────────────────
  useEffect(() => {
    if (open) {
      startListening()
    } else {
      window.speechSynthesis.cancel()
      if (silenceTimerRef.current) { clearTimeout(silenceTimerRef.current); silenceTimerRef.current = null }
      if (voiceStateRef.current === 'recording') toggle()
      setSheetState('idle')
      setDisplayTranscript('')
      setProcessingHint('')
      setResultPaths([])
      setResultContent(null)
      setErrorCard(null)
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  // ── State dot ────────────────────────────────────────────────────────────
  const dotColor = {
    idle:       'var(--color-border)',
    listening:  'var(--color-accent)',
    processing: 'var(--color-warning, #f59e0b)',
    speaking:   'var(--color-success, #22c55e)',
  }[sheetState]

  const dotPulse = sheetState === 'listening' || sheetState === 'speaking'

  const stateLabel = {
    idle:       t('live_chat.idle'),
    listening:  t('live_chat.listening'),
    processing: t('live_chat.thinking'),
    speaking:   t('live_chat.speaking'),
  }[sheetState]

  // ── Render ───────────────────────────────────────────────────────────────
  if (!open) return null

  return (
    <>
      {/* Floating bottom bar — no backdrop, UI remains interactive */}
      <div
        style={{
          position: 'fixed',
          left: 16, right: 16, bottom: 16,
          zIndex: 901,
          background: 'var(--color-bg-secondary, #1e1e2e)',
          border: '1px solid var(--color-border)',
          borderRadius: 16,
          boxShadow: '0 4px 24px rgba(0,0,0,0.45)',
          display: 'flex',
          flexDirection: 'column',
          padding: '0 0 4px',
        }}
        onClick={e => e.stopPropagation()}
      >
        {/* Status row */}
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '8px 16px' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <div
              style={{
                width: 12, height: 12, borderRadius: '50%',
                background: dotColor,
                animation: dotPulse ? 'livechat-pulse 1.2s ease-in-out infinite' : undefined,
                flexShrink: 0,
              }}
            />
            <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
              <span style={{ fontSize: 14, color: 'var(--color-text)', fontWeight: 500 }}>
                {stateLabel}
                {activePattern && sheetState === 'listening' && (
                  <span style={{ fontSize: 11, color: 'var(--color-accent)', marginLeft: 8, opacity: 0.8 }}>
                    ⚡
                  </span>
                )}
              </span>
              {displayTranscript && (
                <span style={{ fontSize: 12, color: 'var(--color-text-muted)', maxWidth: 240, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {displayTranscript}
                </span>
              )}
              {processingHint && !displayTranscript && (
                <span style={{ fontSize: 11, color: 'var(--color-text-muted)', fontStyle: 'italic', maxWidth: 240, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {processingHint}
                </span>
              )}
            </div>
          </div>

          <div style={{ display: 'flex', gap: 8 }}>
            <button
              style={{
                background: 'none',
                border: '1px solid var(--color-border)',
                borderRadius: 20,
                padding: '4px 14px',
                cursor: 'pointer',
                fontSize: 12,
                color: 'var(--color-text-muted)',
                whiteSpace: 'nowrap',
              }}
              onClick={() => {
                if (sheetState === 'idle') {
                  startListening()
                } else {
                  window.speechSynthesis.cancel()
                  if (silenceTimerRef.current) { clearTimeout(silenceTimerRef.current); silenceTimerRef.current = null }
                  if (voiceStateRef.current === 'recording') toggle()
                  setSheetState('idle')
                }
              }}
            >
              {sheetState === 'idle' ? t('live_chat.resume') : t('live_chat.pause')}
            </button>
            <button
              style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--color-text-muted)', fontSize: 16, padding: '4px 6px', lineHeight: 1 }}
              onClick={onClose}
            >
              ✕
            </button>
          </div>
        </div>

        {/* Error card */}
        {errorCard && (
          <div style={{
            margin: '0 16px 8px',
            background: 'rgba(239,68,68,0.12)',
            border: '1px solid rgba(239,68,68,0.3)',
            borderRadius: 8,
            padding: '8px 12px',
            fontSize: 12,
            color: 'var(--color-error, #ef4444)',
          }}>
            {errorCard}
          </div>
        )}

        {/* Debug text input */}
        <div style={{ display: 'flex', gap: 6, padding: '0 16px 8px' }}>
          <input
            type="text"
            value={textInput}
            onChange={e => setTextInput(e.target.value)}
            onKeyDown={e => {
              if (e.key === 'Enter' && textInput.trim()) {
                const q = textInput.trim()
                setTextInput('')
                sendToLLM(q)
              }
            }}
            placeholder="輸入文字測試（Enter 送出）"
            style={{
              flex: 1,
              background: 'var(--color-bg-tertiary, #2a2a3e)',
              border: '1px solid var(--color-border)',
              borderRadius: 8,
              padding: '6px 10px',
              fontSize: 13,
              color: 'var(--color-text)',
              outline: 'none',
            }}
          />
          <button
            onClick={() => {
              const q = textInput.trim()
              if (!q) return
              setTextInput('')
              sendToLLM(q)
            }}
            style={{
              background: 'var(--color-accent)',
              border: 'none',
              borderRadius: 8,
              padding: '6px 12px',
              cursor: 'pointer',
              fontSize: 13,
              color: '#fff',
              whiteSpace: 'nowrap',
            }}
          >
            送出
          </button>
        </div>

        {/* Result panel (search content + note paths) */}
        {(resultContent || resultPaths.length > 0) && (
          <div style={{ margin: '0 12px 10px', border: '1px solid var(--color-border)', borderRadius: 10, overflow: 'hidden' }}>
            {/* Header / toggle */}
            <button
              onClick={() => setResultExpanded(v => !v)}
              style={{
                width: '100%',
                display: 'flex', alignItems: 'center', justifyContent: 'space-between',
                background: 'var(--color-bg-tertiary, #2a2a3e)',
                border: 'none', cursor: 'pointer',
                padding: '6px 12px',
                fontSize: 12, color: 'var(--color-text-muted)',
              }}
            >
              <span>查詢結果</span>
              <span style={{ fontSize: 10, opacity: 0.7 }}>{resultExpanded ? '▲' : '▼'}</span>
            </button>
            {resultExpanded && (
              <div style={{ maxHeight: 220, overflowY: 'auto', padding: '8px 12px', display: 'flex', flexDirection: 'column', gap: 6 }}>
                {resultContent && (
                  <p style={{ margin: 0, fontSize: 12, color: 'var(--color-text)', lineHeight: 1.6, whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}>
                    {resultContent}
                  </p>
                )}
                {resultPaths.map(p => (
                  <div
                    key={p}
                    style={{
                      padding: '5px 10px',
                      borderRadius: 6,
                      cursor: 'pointer',
                      fontSize: 13,
                      color: 'var(--color-accent)',
                      background: 'rgba(var(--color-accent-rgb, 139,92,246),0.08)',
                      display: 'flex', alignItems: 'center', gap: 6,
                    }}
                    onClick={() => { onOpenNote(p); onClose() }}
                  >
                    <span style={{ fontSize: 11 }}>📄</span>
                    <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                      {p.split('/').pop()?.replace('.md', '') ?? p}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
      </div>

      <style>{`
        @keyframes livechat-pulse {
          0%, 100% { opacity: 1; transform: scale(1); }
          50% { opacity: 0.5; transform: scale(0.85); }
        }
      `}</style>
    </>
  )
}
