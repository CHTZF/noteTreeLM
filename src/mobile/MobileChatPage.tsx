import { useState, useRef, useEffect, useCallback } from 'react'
import { wsUrl } from './mobileApi'

interface Message {
  role: 'user' | 'assistant' | 'notice'
  content: string
}

interface Props {
  vaultId: string
  onUnpair: () => void
}

export default function MobileChatPage({ vaultId, onUnpair }: Props) {
  const [messages, setMessages] = useState<Message[]>([])
  const [input, setInput] = useState('')
  const [isStreaming, setIsStreaming] = useState(false)
  const [conversationId, setConversationId] = useState<string | null>(null)
  const [toolDisplay, setToolDisplay] = useState<string | null>(null)

  const streamingRef = useRef('')
  const sessionIdRef = useRef<string | null>(null)
  const wsRef = useRef<WebSocket | null>(null)
  const conversationIdRef = useRef<string | null>(null)
  const bottomRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)

  // Keep ref in sync with state so callbacks always see latest value
  useEffect(() => { conversationIdRef.current = conversationId }, [conversationId])

  // Auto-scroll
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages, isStreaming])

  // Connect WebSocket once on mount, keep alive for the page lifetime
  useEffect(() => {
    const url = wsUrl()
    const ws = new WebSocket(url)
    wsRef.current = ws

    ws.onmessage = (e) => {
      try {
        const msg = JSON.parse(e.data)
        if (msg.type === 'token') {
          streamingRef.current += msg.data
          setMessages(prev => {
            const last = prev[prev.length - 1]
            if (last?.role === 'assistant') {
              return [...prev.slice(0, -1), { role: 'assistant', content: streamingRef.current }]
            }
            return prev
          })
        } else if (msg.type === 'done') {
          const final = streamingRef.current || msg.data || ''
          streamingRef.current = ''
          if (!conversationIdRef.current && msg.conversation_id) {
            setConversationId(msg.conversation_id)
          }
          setIsStreaming(false)
          setToolDisplay(null)
          setMessages(prev => {
            const last = prev[prev.length - 1]
            if (last?.role === 'assistant') {
              return [...prev.slice(0, -1), { role: 'assistant', content: final }]
            }
            return prev
          })
        } else if (msg.type === 'tool') {
          setToolDisplay(msg.data?.display ?? null)
        } else if (msg.type === 'cancelled') {
          streamingRef.current = ''
          setIsStreaming(false)
          setToolDisplay(null)
          setMessages(prev => prev.filter(m => m.content !== '__streaming__'))
        } else if (msg.type === 'error') {
          streamingRef.current = ''
          setIsStreaming(false)
          setToolDisplay(null)
          setMessages(prev => {
            const filtered = prev.filter(m => m.content !== '__streaming__')
            return [...filtered, { role: 'notice', content: `錯誤：${msg.message}` }]
          })
        }
      } catch { /* ignore parse errors */ }
    }

    ws.onerror = () => {
      streamingRef.current = ''
      setIsStreaming(false)
      setToolDisplay(null)
      setMessages(prev => {
        const filtered = prev.filter(m => m.content !== '__streaming__')
        return [...filtered, { role: 'notice', content: '連線錯誤，請重新整理頁面' }]
      })
    }

    return () => { ws.close() }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const sendMessage = useCallback(() => {
    const ws = wsRef.current
    const text = input.trim()
    if (!text || isStreaming || !ws || ws.readyState !== WebSocket.OPEN) return

    setInput('')
    setMessages(prev => [...prev, { role: 'user', content: text }])
    setIsStreaming(true)
    streamingRef.current = ''
    setMessages(prev => [...prev, { role: 'assistant', content: '__streaming__' }])

    const sessionId = crypto.randomUUID()
    sessionIdRef.current = sessionId

    ws.send(JSON.stringify({
      type: 'run',
      vault_id: vaultId,
      session_id: sessionId,
      input: text,
      conversation_id: conversationIdRef.current ?? undefined,
    }))
  }, [input, isStreaming, vaultId])

  const cancel = useCallback(() => {
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: 'cancel' }))
    }
  }, [])

  function handleKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      sendMessage()
    }
  }

  return (
    <div style={styles.root}>
      {/* Header */}
      <div style={styles.header}>
        <span style={styles.headerTitle}>noteTreeLM</span>
        <button style={styles.iconBtn} onClick={onUnpair} title="登出">⎋</button>
      </div>

      {/* Message list */}
      <div style={styles.messageList}>
        {messages.length === 0 && (
          <div style={styles.empty}>有什麼我可以幫你的？</div>
        )}
        {messages.map((m, i) => (
          <div key={i} style={{
            ...styles.bubble,
            ...(m.role === 'user' ? styles.userBubble : m.role === 'notice' ? styles.noticeBubble : styles.aiBubble),
          }}>
            {m.content === '__streaming__'
              ? <span style={styles.cursor}>▋</span>
              : m.content
            }
          </div>
        ))}
        {toolDisplay && (
          <div style={styles.toolBadge}>🔧 {toolDisplay}</div>
        )}
        <div ref={bottomRef} />
      </div>

      {/* Input area */}
      <div style={styles.inputBar}>
        <textarea
          ref={inputRef}
          style={styles.textarea}
          value={input}
          onChange={e => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder="輸入訊息…"
          rows={1}
          disabled={isStreaming}
        />
        {isStreaming
          ? <button style={{ ...styles.sendBtn, background: '#ff453a' }} onClick={cancel}>■</button>
          : <button style={styles.sendBtn} onClick={sendMessage} disabled={!input.trim()}>↑</button>
        }
      </div>
    </div>
  )
}

const styles: Record<string, React.CSSProperties> = {
  root: {
    display: 'flex',
    flexDirection: 'column',
    height: '100dvh',
    background: '#000',
    color: '#fff',
    fontFamily: '-apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif',
    fontSize: 15,
  },
  header: {
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'space-between',
    padding: '12px 16px',
    paddingTop: 'max(12px, env(safe-area-inset-top))',
    background: '#1c1c1e',
    borderBottom: '1px solid #2c2c2e',
    flexShrink: 0,
  },
  headerTitle: {
    fontSize: 17,
    fontWeight: 600,
  },
  iconBtn: {
    background: 'none',
    border: 'none',
    color: '#8e8e93',
    fontSize: 20,
    cursor: 'pointer',
    padding: '4px 8px',
  },
  messageList: {
    flex: 1,
    overflowY: 'auto',
    padding: '16px 12px',
    display: 'flex',
    flexDirection: 'column',
    gap: 10,
  },
  empty: {
    textAlign: 'center',
    color: '#48484a',
    marginTop: 80,
    fontSize: 16,
  },
  bubble: {
    maxWidth: '85%',
    padding: '10px 14px',
    borderRadius: 18,
    lineHeight: 1.5,
    whiteSpace: 'pre-wrap',
    wordBreak: 'break-word',
  },
  userBubble: {
    alignSelf: 'flex-end',
    background: '#0a84ff',
    color: '#fff',
    borderBottomRightRadius: 4,
  },
  aiBubble: {
    alignSelf: 'flex-start',
    background: '#2c2c2e',
    color: '#fff',
    borderBottomLeftRadius: 4,
  },
  noticeBubble: {
    alignSelf: 'center',
    background: 'transparent',
    color: '#8e8e93',
    fontSize: 12,
    padding: '4px 0',
  },
  cursor: {
    animation: 'blink 1s step-end infinite',
  },
  toolBadge: {
    alignSelf: 'flex-start',
    background: '#2c2c2e',
    color: '#8e8e93',
    fontSize: 12,
    padding: '4px 10px',
    borderRadius: 12,
  },
  inputBar: {
    display: 'flex',
    alignItems: 'flex-end',
    gap: 8,
    padding: '8px 12px',
    paddingBottom: 'max(8px, env(safe-area-inset-bottom))',
    background: '#1c1c1e',
    borderTop: '1px solid #2c2c2e',
    flexShrink: 0,
  },
  textarea: {
    flex: 1,
    background: '#2c2c2e',
    border: '1px solid #3a3a3c',
    borderRadius: 20,
    padding: '10px 16px',
    color: '#fff',
    fontSize: 15,
    resize: 'none',
    outline: 'none',
    maxHeight: 120,
    overflowY: 'auto',
    lineHeight: 1.4,
  },
  sendBtn: {
    width: 36,
    height: 36,
    borderRadius: '50%',
    background: '#0a84ff',
    color: '#fff',
    border: 'none',
    fontSize: 18,
    cursor: 'pointer',
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'center',
    flexShrink: 0,
  },
}
