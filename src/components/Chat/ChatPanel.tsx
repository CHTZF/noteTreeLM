import { useState, useRef, useEffect, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useSettingsStore } from '../../stores/settingsStore'
import { useEditorStore } from '../../stores/editorStore'
import { useDebugStore } from '../../stores/debugStore'

interface Message {
  role: 'user' | 'assistant' | 'tool'
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
  const [useNoteContext, setUseNoteContext] = useState(false)
  const [useVaultTools, setUseVaultTools] = useState(false)
  const [error, setError] = useState('')

  const bottomRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const streamingRef = useRef('')
  const tokenCountRef = useRef(0)

  const log = useCallback((msg: string) => addLog('chat', 'info', msg), [addLog])
  const err = useCallback((msg: string) => addLog('chat', 'error', msg), [addLog])

  const isConfigured = !!settings.llama_cli_path && !!settings.llm_model_path
  const vaultConfigured = !!settings.vault_path

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

  // Vault 未設定時關閉 vault tools
  useEffect(() => {
    if (!vaultConfigured) setUseVaultTools(false)
  }, [vaultConfigured])

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

    // 只把 user/assistant 訊息送給 LLM（tool 是顯示用的）
    const llmMessages = allMessages
      .filter((m) => m.role !== 'tool')
      .map((m) => ({ role: m.role, content: m.content }))

    log(`▶ 傳送訊息（${text.length} 字），模式: ${useVaultTools ? 'agent' : 'chat'}`)

    let unlistenToken: (() => void) | undefined
    let unlistenDone: (() => void) | undefined
    let unlistenToolCall: (() => void) | undefined

    try {
      if (useVaultTools) {
        // ── Agent 模式 ──────────────────────────────────────────────────────
        // 監聽工具調用事件，即時顯示在對話中
        unlistenToolCall = await listen<string>('agent:tool_call', (event) => {
          setMessages((prev) => [...prev, { role: 'tool', content: event.payload }])
        })

        const noteContext =
          useNoteContext && currentPath && noteContent
            ? noteContent.slice(0, 4000)
            : undefined

        if (noteContext) log(`  帶入筆記上下文（${noteContext.length} 字元）`)
        log('  呼叫 invoke("agent_chat")')

        const responseText = await invoke<string>('agent_chat', {
          messages: llmMessages,
          system: noteContext,
        })

        const finalContent = responseText || ''
        log(`✓ agent_chat 完成，回覆 ${finalContent.length} 字元`)
        setMessages((prev) => [...prev, { role: 'assistant', content: finalContent }])
      } else {
        // ── 一般串流模式 ─────────────────────────────────────────────────────
        const system =
          useNoteContext && currentPath && noteContent
            ? `你是一個筆記助手。以下是使用者目前開啟的筆記內容，請根據此內容協助回答問題：\n\n${noteContent.slice(0, 4000)}`
            : undefined

        if (system) log(`  帶入筆記上下文（${system.length} 字元）`)

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
      }
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
      setIsStreaming(false)
      setStreamingText('')
      streamingRef.current = ''
    }
  }, [input, isStreaming, messages, useNoteContext, useVaultTools, currentPath, noteContent, log, err])

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
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', background: 'var(--color-bg-base)' }}>
      {/* Header */}
      <div style={{
        display: 'flex', alignItems: 'center', justifyContent: 'space-between',
        padding: '8px 12px', borderBottom: '1px solid var(--color-border)', flexShrink: 0,
      }}>
        <span style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)', letterSpacing: '0.05em' }}>
          CHAT
        </span>
        <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
          {/* Vault 工具 toggle */}
          <label style={{
            display: 'flex', alignItems: 'center', gap: '5px', fontSize: '11px',
            cursor: vaultConfigured ? 'pointer' : 'not-allowed',
            color: useVaultTools ? 'var(--color-accent)' : 'var(--color-text-muted)',
            opacity: vaultConfigured ? 1 : 0.35,
          }}>
            <input
              type="checkbox"
              checked={useVaultTools}
              onChange={(e) => setUseVaultTools(e.target.checked)}
              disabled={!vaultConfigured}
              style={{ cursor: 'pointer', accentColor: 'var(--color-accent)', width: '12px', height: '12px' }}
            />
            Vault 工具
          </label>
          {/* 帶入筆記 toggle */}
          <label style={{
            display: 'flex', alignItems: 'center', gap: '5px', fontSize: '11px',
            cursor: currentPath ? 'pointer' : 'not-allowed',
            color: useNoteContext ? 'var(--color-accent)' : 'var(--color-text-muted)',
          }}>
            <input
              type="checkbox"
              checked={useNoteContext}
              onChange={(e) => setUseNoteContext(e.target.checked)}
              disabled={!currentPath}
              style={{ cursor: 'pointer', accentColor: 'var(--color-accent)', width: '12px', height: '12px' }}
            />
            帶入筆記
          </label>
          {messages.length > 0 && (
            <button
              onClick={clearChat}
              style={{
                fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
                background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
                color: 'var(--color-text-secondary)', cursor: 'pointer',
              }}
            >清除</button>
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

      {/* 訊息列表 */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '8px 12px', display: 'flex', flexDirection: 'column', gap: '6px' }}>
        {messages.length === 0 && !isStreaming && (
          <div style={{
            display: 'flex', alignItems: 'center', justifyContent: 'center',
            height: '100%', color: 'var(--color-text-muted)', fontSize: '12px',
            textAlign: 'center', lineHeight: 1.6,
          }}>
            {useVaultTools
              ? '與 Vault 助手對話\n可搜索、新增、編輯 Vault 中的筆記與資料夾'
              : '與本地 LLM 對話'}
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
          placeholder={isConfigured
            ? (useVaultTools ? '搜索筆記、新增/編輯筆記或資料夾…' : '輸入訊息…')
            : '請先設定 llama CLI'}
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
        <button
          onClick={send}
          disabled={isStreaming || !input.trim() || !isConfigured}
          style={{
            padding: '0 14px', height: '34px', borderRadius: '6px', flexShrink: 0,
            background: isStreaming ? 'var(--color-bg-overlay)' : 'var(--color-accent)',
            color: isStreaming ? 'var(--color-text-muted)' : 'white',
            border: 'none', fontSize: '13px',
            cursor: isStreaming || !input.trim() || !isConfigured ? 'not-allowed' : 'pointer',
            opacity: isStreaming || !input.trim() || !isConfigured ? 0.5 : 1,
          }}
        >
          {isStreaming ? '…' : '送出'}
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
