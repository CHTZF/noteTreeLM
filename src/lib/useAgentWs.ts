import { useCallback } from 'react'

type EventHandler = (data: unknown) => void

export interface RunParams {
  vaultId: string
  input: string
  conversationId?: string
  platform: 'desktop' | 'mobile'
  activeNote?: string
  selection?: string
  uiLanguage?: string
  agent?: string
}

export interface RunResult {
  sessionId: string
  conversationId: string
}

function getWsUrl(): string {
  const token = localStorage.getItem('daemon_token') ?? ''
  return `ws://127.0.0.1:7787/api/v1/ws${token ? `?token=${encodeURIComponent(token)}` : ''}`
}

// ── App-scoped singleton ───────────────────────────────────────────────────────
// The WebSocket is opened once when this module loads and never closed until the
// OS kills the process. Component mount/unmount has no effect on the socket.
// Reconnect runs automatically every 3 s when the daemon is not yet ready.

const _handlers = new Map<string, Set<EventHandler>>()
const _doneResolvers: Array<(result: RunResult) => void> = []
let _pendingSessionId = ''
let _ws: WebSocket | null = null
let _reconnectTimer: ReturnType<typeof setTimeout> | null = null

function connect() {
  if (_reconnectTimer) { clearTimeout(_reconnectTimer); _reconnectTimer = null }
  const sock = new WebSocket(getWsUrl())
  _ws = sock

  sock.onmessage = (e: MessageEvent) => {
    try {
      const msg = JSON.parse(e.data as string) as {
        event: string
        data: unknown
        conversation_id?: string
      }
      const set = _handlers.get(msg.event)
      if (set) { for (const h of set) h(msg.data) }

      if (msg.event === 'llm:done' && _doneResolvers.length > 0) {
        const resolve = _doneResolvers.shift()!
        resolve({ sessionId: _pendingSessionId, conversationId: msg.conversation_id ?? '' })
      }
      if (msg.event === 'llm:error' && _doneResolvers.length > 0) {
        const resolve = _doneResolvers.shift()!
        resolve({ sessionId: _pendingSessionId, conversationId: '' })
      }
    } catch { /* ignore parse errors */ }
  }

  sock.onclose = () => {
    _reconnectTimer = setTimeout(connect, 3000)
  }

  sock.onerror = () => {}
}

// Connect immediately on module load
connect()

// ── Hook ──────────────────────────────────────────────────────────────────────
// Components call useAgentWs() to get stable function references.
// No setup/teardown on mount/unmount — the singleton lives independently.

export function useAgentWs() {
  /** Register a handler for an event. Returns an unsubscribe function. */
  const on = useCallback((event: string, handler: EventHandler): (() => void) => {
    if (!_handlers.has(event)) _handlers.set(event, new Set())
    _handlers.get(event)!.add(handler)
    return () => _handlers.get(event)?.delete(handler)
  }, [])

  /** Send a run request. Resolves when llm:done is received (or on cancel). */
  const run = useCallback((params: RunParams): Promise<RunResult> => {
    const sessionId = crypto.randomUUID()
    _pendingSessionId = sessionId
    const msg = JSON.stringify({
      type: 'run',
      vault_id: params.vaultId,
      session_id: sessionId,
      input: params.input,
      conversation_id: params.conversationId,
      platform: params.platform,
      active_note: params.activeNote,
      selection: params.selection,
      ui_language: params.uiLanguage,
      agent: params.agent,
    })
    return new Promise((resolve) => {
      _doneResolvers.push(resolve)
      if (!_ws) { resolve({ sessionId, conversationId: '' }); return }
      if (_ws.readyState === WebSocket.OPEN) {
        _ws.send(msg)
      } else if (_ws.readyState === WebSocket.CONNECTING) {
        const prevOnOpen = _ws.onopen
        _ws.onopen = (e) => {
          if (prevOnOpen) (prevOnOpen as (e: Event) => void)(e)
          _ws!.send(msg)
        }
      } else {
        resolve({ sessionId, conversationId: '' })
      }
    })
  }, [])

  /** Cancel the current run. */
  const cancel = useCallback(() => {
    if (_ws && _ws.readyState === WebSocket.OPEN) {
      _ws.send(JSON.stringify({ type: 'cancel' }))
    }
    // Resolve pending promises so callers don't hang forever
    for (const resolve of _doneResolvers) {
      resolve({ sessionId: _pendingSessionId, conversationId: '' })
    }
    _doneResolvers.length = 0
  }, [])

  /** Confirm or deny a write-tool request. */
  const confirm = useCallback((approved: boolean) => {
    if (_ws && _ws.readyState === WebSocket.OPEN) {
      _ws.send(JSON.stringify({ type: 'confirm', approved }))
    }
  }, [])

  return { on, run, cancel, confirm }
}
