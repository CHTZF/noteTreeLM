import { useState, useRef, useEffect, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useSettingsStore } from '../../stores/settingsStore'
import { useEditorStore } from '../../stores/editorStore'
import { useDebugStore } from '../../stores/debugStore'
import { toast } from '../common/Toast'
import { useVoiceRecorder } from '../../hooks/useVoiceRecorder'
import VoiceOverlay from '../common/VoiceOverlay'

interface Message {
  role: 'user' | 'assistant' | 'tool' | 'notice'
  content: string
}

export default function ChatPanel() {
  const { settings } = useSettingsStore()
  const { content: noteContent, currentPath } = useEditorStore()
  const { addLog } = useDebugStore()

  const [messages, setMessages] = useState<Message[]>([])
  const [input, setInput] = useState('')
  const [isStreaming, setIsStreaming] = useState(false)
  const [streamingText, setStreamingText] = useState('')
  const useNoteContext = !!settings.chat_auto_include_note
  const writeConfirmMode = (settings.write_confirm_mode ?? 'always') as 'always' | 'once' | 'never'
  const [pendingWriteDisplay, setPendingWriteDisplay] = useState<string | null>(null)
  const [error, setError] = useState('')
  const [isCompressing, setIsCompressing] = useState(false)
  const [lastMemoryPath, setLastMemoryPath] = useState<string | null>(null)

  // ─── Voice overlay ────────────────────────────────────────────────────────────
  const [voiceTranscript, setVoiceTranscript]   = useState('')
  const [voicePreview, setVoicePreview]         = useState<string | null>(null)
  const [showVoiceOverlay, setShowVoiceOverlay] = useState(false)
  const pendingVoiceActionRef = useRef<'confirm' | 'discard' | null>(null)

  const bottomRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const streamingRef = useRef('')
  const tokenCountRef = useRef(0)
  const justCompressedRef = useRef(false)
  const sessionWriteApprovedRef = useRef(false)

  const log = useCallback((msg: string) => addLog('chat', 'info', msg), [addLog])
  const err = useCallback((msg: string) => addLog('chat', 'error', msg), [addLog])

  const isConfigured = !!settings.llama_cli_path && !!settings.llm_model_path
  const whisperConfigured = !!settings.whisper_cli_path && !!settings.whisper_model_path
  const vaultConfigured = !!settings.vault_path

  // 語音轉文字：逐字累積到 overlay buffer（不直接寫入輸入框）
  const handleTranscript = useCallback((text: string) => {
    if (!text) return
    setVoiceTranscript((prev) => prev + text)
  }, [])
  // 臨時預覽：null = 清除，string = 更新預覽文字
  const handlePreview = useCallback((text: string | null) => {
    setVoicePreview(text)
  }, [])
  const previewEnabled          = settings.voice_preview_enabled !== false
  const noiseSuppressionEnabled = settings.voice_noise_suppression !== false
  const previewIntervalMs       = settings.voice_preview_interval ?? 5000
  const whisperLanguage         = settings.whisper_language ?? 'auto'
  const { state: voiceState, isSpeaking: voiceIsSpeaking, toggle: toggleVoice } = useVoiceRecorder(
    handleTranscript,
    previewEnabled ? handlePreview : undefined,
    noiseSuppressionEnabled,
    previewIntervalMs,
    whisperLanguage,
  )

  // 錄音開始時顯示 overlay，並重置轉錄文字與預覽
  useEffect(() => {
    if (voiceState === 'recording') {
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(true)
    }
  }, [voiceState])

  // 有待執行動作（confirm/discard）時，等辨識完成後執行
  useEffect(() => {
    const action = pendingVoiceActionRef.current
    if (!action) return
    if (voiceState === 'done' || voiceState === 'idle') {
      pendingVoiceActionRef.current = null
      const text = voiceTranscript
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(false)
      if (action === 'confirm' && text) {
        setInput((prev) => {
          const sep = prev && !prev.endsWith(' ') ? ' ' : ''
          return prev + sep + text
        })
      }
    }
  }, [voiceState, voiceTranscript])

  // 語音帶入時自動調整 textarea 高度
  useEffect(() => {
    const t = inputRef.current
    if (!t) return
    t.style.height = 'auto'
    t.style.height = Math.min(t.scrollHeight, 120) + 'px'
    t.scrollTop = t.scrollHeight
  }, [input])

  // 持續監聽 llm:stderr → 寫入 debug
  // 使用 cancelled flag 解決 React StrictMode 下 async listen() 的 race condition
  // （cleanup 可能在 Promise resolve 前執行，導致舊 listener 未被移除而重複觸發）
  useEffect(() => {
    let cancelled = false
    let unlisten: (() => void) | undefined
    listen<string>('llm:stderr', (event) => {
      useDebugStore.getState().addLog('llm', 'warn', event.payload.trimEnd())
    }).then((fn) => {
      if (cancelled) fn() // 已 cleanup，立即取消這個 listener
      else unlisten = fn
    })
    return () => {
      cancelled = true
      unlisten?.()
    }
  }, []) // addLog 是 Zustand stable reference，不需要作為 dep

  // 自動捲動到底部
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages.length, streamingText])

  // vault_path 變化時（包含首次設定、切換 vault）重置 lastMemoryPath 並重新查詢
  // 確保切換 vault 後不會帶入舊 vault 的記憶路徑
  useEffect(() => {
    if (!vaultConfigured) {
      setLastMemoryPath(null)
      return
    }
    setLastMemoryPath(null) // 先清除，避免短暫帶入舊路徑
    invoke<Array<{ path: string }>>('query_memory', { keywords: [], limit: 1 })
      .then((results) => {
        if (results.length > 0) setLastMemoryPath(results[0].path)
      })
      .catch(() => {})
  }, [settings.vault_path])

  const handleWriteConfirm = useCallback(async (approved: boolean) => {
    if (approved && writeConfirmMode === 'once') {
      sessionWriteApprovedRef.current = true
    }
    setPendingWriteDisplay(null)
    await invoke('confirm_write_tool', { approved })
  }, [writeConfirmMode])

  const send = useCallback(async () => {
    const text = input.trim()
    if (!text || isStreaming) return

    const userMsg: Message = { role: 'user', content: text }
    const allMessages = [...messages, userMsg]
    setMessages(allMessages)
    setInput('')
    setIsStreaming(true)
    setStreamingText('')
    setError('')
    streamingRef.current = ''
    tokenCountRef.current = 0

    // 只把 user/assistant 訊息送給 LLM（tool/notice 只做 UI 顯示，不進入 context）
    const llmMessages = allMessages
      .filter((m) => m.role === 'user' || m.role === 'assistant')
      .map((m) => ({ role: m.role, content: m.content }))

    log(`▶ 傳送訊息（${text.length} 字）`)

    let unlistenToken: (() => void) | undefined
    let unlistenDone: (() => void) | undefined
    let unlistenToolCall: (() => void) | undefined
    let unlistenWriteReq: (() => void) | undefined

    try {
      const notePart =
        useNoteContext && currentPath && noteContent
          ? `你是一個筆記助手。以下是使用者目前開啟的筆記內容，請根據此內容協助回答問題：\n\n${noteContent.slice(0, 4000)}`
          : null

      // 若有記憶存檔，由 resolve_memory_context 查詢（純 Rust，< 100ms）
      let memoryPart: string | null = null
      if (lastMemoryPath) {
        try {
          const memorySummary = await invoke<string>('resolve_memory_context', { query: text })
          if (memorySummary) {
            memoryPart = `以下是相關的過去對話記憶（供參考）：\n\n${memorySummary}`
            log(`  帶入記憶摘要（${memorySummary.length} 字元）`)
          } else {
            log('  resolve_memory_context：無相關記憶，略過注入')
          }
        } catch (e) {
          const msg = typeof e === 'string' ? e : e instanceof Error ? e.message : String(e)
          err('resolve_memory_context 查詢失敗：' + msg)
        }
      }

      const system = [notePart, memoryPart].filter(Boolean).join('\n\n') || undefined
      if (system) log(`  帶入 system 上下文（${system.length} 字元）`)

      // 監聽工具調用顯示
      unlistenToolCall = await listen<string>('agent:tool_call', (event) => {
        setMessages((prev) => [...prev, { role: 'tool', content: event.payload }])
      })

      // 監聽寫入確認請求
      unlistenWriteReq = await listen<string>('agent:write_request', async (event) => {
        const display = event.payload
        if (writeConfirmMode === 'never') {
          await invoke('confirm_write_tool', { approved: true })
          return
        }
        if (writeConfirmMode === 'once' && sessionWriteApprovedRef.current) {
          await invoke('confirm_write_tool', { approved: true })
          return
        }
        // 'always' 或 'once' 首次：顯示確認 UI
        setPendingWriteDisplay(display)
      })

      unlistenToken = await listen<string>('llm:token', (event) => {
        streamingRef.current += event.payload
        tokenCountRef.current += event.payload.length
        setStreamingText(streamingRef.current)
        if (tokenCountRef.current === event.payload.length) {
          log('✓ 開始收到 llm:token 串流')
        }
      })
      unlistenDone = await listen('llm:done', () => {
        log(`⏹ llm:done 事件收到，共 ${tokenCountRef.current} 字元`)
      })

      log('  呼叫 invoke("stream_chat")')
      const responseText = await invoke<string>('stream_chat', {
        messages: llmMessages,
        system,
      })

      const finalContent = responseText || streamingRef.current
      log(`✓ stream_chat 完成，回覆 ${finalContent.length} 字元`)
      setMessages((prev) => [...prev, { role: 'assistant', content: finalContent }])
    } catch (e: unknown) {
      const msg =
        typeof e === 'string'
          ? e
          : e instanceof Error
            ? e.message
            : (() => { try { return JSON.stringify(e, null, 2) } catch { return String(e) } })()
      err(`invoke 失敗：\n${msg}`)
      setError(msg)
    } finally {
      unlistenToken?.()
      unlistenDone?.()
      unlistenToolCall?.()
      unlistenWriteReq?.()
      setPendingWriteDisplay(null)
      setIsStreaming(false)
      setStreamingText('')
      streamingRef.current = ''
    }
  }, [input, isStreaming, messages, useNoteContext, writeConfirmMode, currentPath, noteContent, lastMemoryPath, log, err])

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      send()
    }
  }

  const clearChat = () => {
    setMessages([])
    setError('')
    setStreamingText('')
    streamingRef.current = ''
    setLastMemoryPath(null)
  }

  // 填入：若仍在錄音/辨識中則先停止，完成後填入輸入框
  const handleVoiceConfirm = useCallback(() => {
    if (voiceState === 'recording') {
      pendingVoiceActionRef.current = 'confirm'
      toggleVoice()
    } else if (voiceState === 'transcribing') {
      pendingVoiceActionRef.current = 'confirm'
    } else {
      const text = voiceTranscript
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(false)
      if (text) {
        setInput((prev) => {
          const sep = prev && !prev.endsWith(' ') ? ' ' : ''
          return prev + sep + text
        })
      }
    }
  }, [voiceState, voiceTranscript, toggleVoice])

  // 捨棄：若仍在錄音/辨識中則先停止，完成後直接關閉 overlay
  const handleVoiceDiscard = useCallback(() => {
    if (voiceState === 'recording') {
      pendingVoiceActionRef.current = 'discard'
      toggleVoice()
    } else if (voiceState === 'transcribing') {
      pendingVoiceActionRef.current = 'discard'
    } else {
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(false)
    }
  }, [voiceState, toggleVoice])

  // 壓縮記憶：把目前對話原文存成 memories/ai_memory_[timestamp].md
  const compressToMemory = useCallback(async () => {
    const toCompress = messages.filter(m => m.role !== 'tool')
    if (toCompress.length === 0 || isCompressing) return
    setIsCompressing(true)
    try {
      const path = await invoke<string>('save_memory_session', { messages: toCompress })
      setLastMemoryPath(path)
      const filename = path.split('/').pop() ?? path
      setMessages([{
        role: 'notice',
        content: `已儲存記憶：${filename}`,
      }])
      toast.success(`記憶已儲存：${filename}`)
      log(`記憶已儲存：${path}`)
    } catch (e: unknown) {
      const msg = typeof e === 'string' ? e : e instanceof Error ? e.message : String(e)
      toast.error('記憶儲存失敗')
      err('save_memory_session 失敗：' + msg)
    } finally {
      setIsCompressing(false)
    }
  }, [messages, isCompressing, log, err])

  // 自動觸發：當訊息數量達到閾值時壓縮記憶
  useEffect(() => {
    const threshold = settings.memory_threshold ?? 20
    if (!settings.enable_auto_memory) return
    if (isStreaming || isCompressing) return
    if (justCompressedRef.current) { justCompressedRef.current = false; return }
    const meaningful = messages.filter(m => m.role === 'user' || m.role === 'assistant').length
    if (meaningful >= threshold) {
      justCompressedRef.current = true
      compressToMemory()
    }
  }, [messages.length, settings.enable_auto_memory, settings.memory_threshold, isStreaming, isCompressing, compressToMemory])

  return (
    <div style={{ position: 'relative', display: 'flex', flexDirection: 'column', height: '100%', background: 'var(--color-bg-base)' }}>
      {/* Voice Overlay */}
      {showVoiceOverlay && (
        <VoiceOverlay
          voiceState={voiceState}
          transcript={voiceTranscript}
          preview={voicePreview}
          previewEnabled={previewEnabled}
          isSpeaking={voiceIsSpeaking}
          onConfirm={handleVoiceConfirm}
          onDiscard={handleVoiceDiscard}
        />
      )}

      {/* Header */}
      <div style={{
        display: 'flex', alignItems: 'center', justifyContent: 'space-between',
        padding: '8px 12px', borderBottom: '1px solid var(--color-border)', flexShrink: 0,
      }}>
        <span style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)', letterSpacing: '0.05em' }}>
          CHAT
        </span>
        <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
          {messages.length > 0 && (
            <>
              <button
                onClick={compressToMemory}
                disabled={isCompressing || isStreaming}
                title="將目前對話儲存為記憶筆記並清空對話"
                style={{
                  fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
                  background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
                  color: isCompressing ? 'var(--color-text-muted)' : 'var(--color-accent)',
                  cursor: isCompressing || isStreaming ? 'not-allowed' : 'pointer',
                  opacity: isCompressing || isStreaming ? 0.5 : 1,
                }}
              >{isCompressing ? '儲存中…' : '壓縮記憶'}</button>
              <button
                onClick={clearChat}
                style={{
                  fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
                  background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
                  color: 'var(--color-text-secondary)', cursor: 'pointer',
                }}
              >清除</button>
            </>
          )}
        </div>
      </div>

      {/* 未設定警告 */}
      {!isConfigured && (
        <div style={{
          margin: '10px 12px', padding: '8px 10px', borderRadius: '6px',
          background: 'rgba(245,158,11,0.08)', border: '1px solid rgba(245,158,11,0.25)',
          fontSize: '12px', color: 'var(--color-warning, #f59e0b)', lineHeight: 1.5,
        }}>
          ⚠ 請先到 <strong>Settings &gt; AI</strong> 設定 llama CLI 路徑與本地模型。
        </div>
      )}

      {/* 接近閾值警告（未開啟自動記憶時顯示） */}
      {(() => {
        const threshold = settings.memory_threshold ?? 20
        const meaningful = messages.filter(m => m.role === 'user' || m.role === 'assistant').length
        const nearLimit = meaningful >= Math.max(threshold - 4, 1) && meaningful < threshold
        return !settings.enable_auto_memory && nearLimit ? (
          <div style={{
            margin: '4px 12px 0', padding: '6px 10px', borderRadius: '6px',
            background: 'rgba(245,158,11,0.08)', border: '1px solid rgba(245,158,11,0.25)',
            fontSize: '11px', color: 'var(--color-warning, #f59e0b)', lineHeight: 1.5,
            display: 'flex', alignItems: 'center', justifyContent: 'space-between',
          }}>
            <span>對話已達 {meaningful} 則，接近閾值 {threshold}，建議壓縮記憶。</span>
            <button
              onClick={compressToMemory}
              disabled={isCompressing}
              style={{
                marginLeft: '8px', fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
                background: 'rgba(245,158,11,0.15)', border: '1px solid rgba(245,158,11,0.4)',
                color: 'var(--color-warning, #f59e0b)', cursor: 'pointer', flexShrink: 0,
              }}
            >立即壓縮</button>
          </div>
        ) : null
      })()}

      {/* 訊息列表 */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '8px 12px', display: 'flex', flexDirection: 'column', gap: '6px' }}>
        {messages.length === 0 && !isStreaming && (
          <div style={{
            display: 'flex', alignItems: 'center', justifyContent: 'center',
            height: '100%', color: 'var(--color-text-muted)', fontSize: '12px',
            textAlign: 'center', lineHeight: 1.6,
          }}>
            {'與本地 LLM 對話（自動使用工具）'}
            <br />
            <span style={{ fontSize: '11px', opacity: 0.7 }}>按 Enter 送出，Shift+Enter 換行</span>
          </div>
        )}

        {messages.map((msg, i) => (
          <MessageBubble key={i} message={msg} />
        ))}

        {/* 串流中 / agent 思考中的 assistant 泡泡 */}
        {isStreaming && (
          <MessageBubble
            message={{ role: 'assistant', content: streamingText }}
            streaming
          />
        )}

        {/* 錯誤訊息 */}
        {error && (
          <div style={{
            padding: '8px 10px', borderRadius: '6px', fontSize: '12px',
            background: 'rgba(239,68,68,0.08)', border: '1px solid rgba(239,68,68,0.2)',
            color: 'var(--color-error, #ef4444)', wordBreak: 'break-all',
          }}>
            ⚠ {error}
          </div>
        )}

        <div ref={bottomRef} />
      </div>

      {/* 寫入確認 Bubble */}
      {pendingWriteDisplay && (
        <div style={{
          margin: '0 8px 8px', padding: '10px 14px', borderRadius: '8px',
          background: 'var(--color-bg-elevated)', border: '1px solid var(--color-accent)',
          fontSize: '13px', flexShrink: 0,
        }}>
          <div style={{ marginBottom: '8px', color: 'var(--color-text-primary)', whiteSpace: 'pre-wrap', fontFamily: 'var(--font-mono, monospace)', fontSize: '12px' }}>
            {pendingWriteDisplay}
          </div>
          <div style={{ fontSize: '12px', color: 'var(--color-text-secondary)', marginBottom: '8px' }}>
            LLM 想執行此寫入操作，是否允許？
          </div>
          <div style={{ display: 'flex', gap: '6px' }}>
            <button
              onClick={() => handleWriteConfirm(true)}
              style={{ padding: '4px 12px', borderRadius: '4px', background: 'var(--color-accent)', color: '#fff', border: 'none', cursor: 'pointer', fontSize: '12px' }}
            >允許</button>
            <button
              onClick={() => handleWriteConfirm(false)}
              style={{ padding: '4px 12px', borderRadius: '4px', background: 'transparent', color: 'var(--color-text-secondary)', border: '1px solid var(--color-border)', cursor: 'pointer', fontSize: '12px' }}
            >拒絕</button>
          </div>
        </div>
      )}

      {/* 輸入區 */}
      <div style={{
        padding: '8px 12px', borderTop: '1px solid var(--color-border)', flexShrink: 0,
        display: 'flex', gap: '6px', alignItems: 'flex-end',
      }}>
        <textarea
          ref={inputRef}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          disabled={isStreaming || !isConfigured}
          placeholder={isConfigured ? '輸入訊息…' : '請先設定 llama CLI'}
          rows={1}
          style={{
            flex: 1, resize: 'none', padding: '7px 10px',
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
            borderRadius: '6px', color: 'var(--color-text-primary)', fontSize: '13px',
            outline: 'none', fontFamily: 'var(--font-sans)', lineHeight: 1.5,
            minHeight: '34px', maxHeight: '120px', overflowY: 'auto',
            opacity: !isConfigured ? 0.5 : 1,
          }}
          onInput={(e) => {
            const t = e.currentTarget
            t.style.height = 'auto'
            t.style.height = Math.min(t.scrollHeight, 120) + 'px'
          }}
        />
        {/* 錄音按鈕 */}
        <button
          onClick={toggleVoice}
          disabled={!whisperConfigured || voiceState === 'transcribing' || showVoiceOverlay}
          title={!whisperConfigured ? '請先到設定頁設定 Whisper' : voiceState === 'recording' ? '停止錄音' : '語音輸入'}
          style={{
            width: '34px', height: '34px', borderRadius: '6px', flexShrink: 0,
            display: 'flex', alignItems: 'center', justifyContent: 'center',
            border: '1px solid var(--color-border)',
            background: voiceState === 'recording'
              ? 'rgba(239,68,68,0.15)'
              : voiceState === 'done'
                ? 'rgba(74,222,128,0.15)'
                : 'var(--color-bg-elevated)',
            color: voiceState === 'recording'
              ? '#ef4444'
              : voiceState === 'done'
                ? '#4ade80'
                : voiceState === 'error'
                  ? '#f59e0b'
                  : 'var(--color-text-muted)',
            cursor: !whisperConfigured || voiceState === 'transcribing' ? 'not-allowed' : 'pointer',
            opacity: !whisperConfigured || voiceState === 'transcribing' ? 0.4 : 1,
            transition: 'background 0.2s, color 0.2s',
          }}
        >
          {voiceState === 'recording' ? (
            // 停止方塊
            <svg width="11" height="11" viewBox="0 0 24 24" fill="currentColor">
              <rect x="3" y="3" width="18" height="18" rx="2"/>
            </svg>
          ) : voiceState === 'transcribing' ? (
            // 轉圈點點
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round">
              <circle cx="12" cy="12" r="9" strokeDasharray="28 56" strokeDashoffset="0">
                <animateTransform attributeName="transform" type="rotate" from="0 12 12" to="360 12 12" dur="0.8s" repeatCount="indefinite"/>
              </circle>
            </svg>
          ) : (
            // 麥克風
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z"/>
              <path d="M19 10v2a7 7 0 0 1-14 0v-2"/>
              <line x1="12" y1="19" x2="12" y2="23"/>
              <line x1="8" y1="23" x2="16" y2="23"/>
            </svg>
          )}
        </button>
        {/* 送出按鈕（箭頭） */}
        <button
          onClick={send}
          disabled={isStreaming || !input.trim() || !isConfigured}
          title="送出"
          style={{
            width: '34px', height: '34px', borderRadius: '6px', flexShrink: 0,
            display: 'flex', alignItems: 'center', justifyContent: 'center',
            background: isStreaming || !input.trim() || !isConfigured ? 'var(--color-bg-overlay)' : 'var(--color-accent)',
            color: isStreaming || !input.trim() || !isConfigured ? 'var(--color-text-muted)' : 'white',
            border: 'none',
            cursor: isStreaming || !input.trim() || !isConfigured ? 'not-allowed' : 'pointer',
            opacity: isStreaming || !input.trim() || !isConfigured ? 0.5 : 1,
          }}
        >
          {isStreaming ? (
            <span style={{ fontSize: '16px', lineHeight: 1 }}>…</span>
          ) : (
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <line x1="12" y1="19" x2="12" y2="5"/>
              <polyline points="5 12 12 5 19 12"/>
            </svg>
          )}
        </button>
      </div>
    </div>
  )
}


function MessageBubble({
  message,
  streaming,
}: {
  message: Message
  streaming?: boolean
}) {
  const isUser = message.role === 'user'
  const isTool = message.role === 'tool'
  const isNotice = message.role === 'notice'

  if (isNotice) {
    return (
      <div style={{
        textAlign: 'center', fontSize: '11px', color: 'var(--color-text-muted)',
        padding: '4px 0', opacity: 0.7,
      }}>
        ── {message.content} ──
      </div>
    )
  }

  if (isTool) {
    return (
      <div style={{ display: 'flex', justifyContent: 'flex-start' }}>
        <div style={{
          maxWidth: '92%', padding: '4px 10px', borderRadius: '6px',
          background: 'var(--color-bg-overlay)',
          color: 'var(--color-text-muted)',
          fontSize: '11px', lineHeight: 1.5,
          fontFamily: 'var(--font-mono, monospace)',
          border: '1px solid var(--color-border)',
          whiteSpace: 'pre-wrap',
        }}>
          {message.content}
        </div>
      </div>
    )
  }

  return (
    <div style={{ display: 'flex', justifyContent: isUser ? 'flex-end' : 'flex-start' }}>
      <div style={{
        maxWidth: '85%', padding: '8px 12px',
        borderRadius: isUser ? '12px 12px 4px 12px' : '12px 12px 12px 4px',
        background: isUser ? 'var(--color-accent)' : 'var(--color-bg-elevated)',
        color: isUser ? 'white' : 'var(--color-text-primary)',
        fontSize: '13px', lineHeight: 1.6, wordBreak: 'break-word',
        border: isUser ? 'none' : '1px solid var(--color-border)',
        whiteSpace: 'pre-wrap',
      }}>
        {message.content}
        {streaming && (
          <span style={{
            display: 'inline-block', width: '2px', height: '14px',
            background: 'var(--color-accent)', marginLeft: '2px', verticalAlign: 'text-bottom',
            animation: 'blink 1s step-start infinite',
          }} />
        )}
        {streaming && (
          <style>{`@keyframes blink { 0%, 100% { opacity: 1 } 50% { opacity: 0 } }`}</style>
        )}
      </div>
    </div>
  )
}
