import { useRef, useEffect, useCallback } from 'react'

type EventHandler = (data: unknown) => void

export interface RunParams {
  vaultId: string
  input: string
  conversationId?: string
  platform: 'desktop' | 'mobile'
  activeNote?: string
  selection?: string
  uiLanguage?: string
}

export interface RunResult {
  sessionId: string
  conversationId: string
}

function getWsUrl(): string {
  const token = localStorage.getItem('daemon_token') ?? ''
  return `ws://127.0.0.1:7787/api/v1/ws${token ? `?token=${encodeURIComponent(token)}` : ''}`
}

/**
 * Shared WebSocket hook for agent chat.
 * Works for both desktop (connects to 127.0.0.1:7787) and mobile (handled separately via mobileApi.wsUrl).
 *
 * Usage:
 *   const { on, run, cancel, confirm } = useAgentWs()
 *
 *   // Register event handlers (returns unlisten fn)
 *   useEffect(() => {
 *     const off = on('llm:token', (data) => { ... })
 *     return off
 *   }, [on])
 *
 *   // Send a message
 *   const result = await run({ vaultId, input, platform: 'desktop', ... })
 *   // result.conversationId available after llm:done
 */
export function useAgentWs() {
  const wsRef = useRef<WebSocket | null>(null)
  const handlersRef = useRef<Map<string, Set<EventHandler>>>(new Map())
  // Resolvers for run() Promises — resolved when llm:done arrives
  const doneResolversRef = useRef<Array<(result: RunResult) => void>>([])
  const pendingSessionRef = useRef<string>('')

  useEffect(() => {
    const url = getWsUrl()
    const ws = new WebSocket(url)
    wsRef.current = ws

    ws.onmessage = (e: MessageEvent) => {
      try {
        const msg = JSON.parse(e.data as string) as {
          event: string
          data: unknown
          conversation_id?: string
        }

        // Dispatch to all registered handlers for this event
        const handlers = handlersRef.current.get(msg.event)
        if (handlers) {
          for (const h of handlers) h(msg.data)
        }

        // Resolve the pending run() Promise on llm:done
        if (msg.event === 'llm:done' && doneResolversRef.current.length > 0) {
          const resolve = doneResolversRef.current.shift()!
          resolve({
            sessionId: pendingSessionRef.current,
            conversationId: msg.conversation_id ?? '',
          })
        }

        // Reject the pending run() Promise on llm:error
        if (msg.event === 'llm:error' && doneResolversRef.current.length > 0) {
          // Resolve with empty conversation_id; callers check for llm:error separately
          const resolve = doneResolversRef.current.shift()!
          resolve({ sessionId: pendingSessionRef.current, conversationId: '' })
        }
      } catch { /* ignore parse errors */ }
    }

    return () => {
      ws.close()
      wsRef.current = null
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  /**
   * Register a handler for an event. Returns an unsubscribe function.
   * Equivalent to Tauri's listen() but synchronous and no-cleanup-needed.
   */
  const on = useCallback((event: string, handler: EventHandler): (() => void) => {
    if (!handlersRef.current.has(event)) {
      handlersRef.current.set(event, new Set())
    }
    handlersRef.current.get(event)!.add(handler)
    return () => handlersRef.current.get(event)?.delete(handler)
  }, [])

  /**
   * Send a run request. Returns a Promise that resolves when llm:done is received.
   * Equivalent to invoke('invoke_agent').
   */
  const run = useCallback((params: RunParams): Promise<RunResult> => {
    const sessionId = crypto.randomUUID()
    pendingSessionRef.current = sessionId
    return new Promise((resolve) => {
      doneResolversRef.current.push(resolve)
      wsRef.current?.send(JSON.stringify({
        type: 'run',
        vault_id: params.vaultId,
        session_id: sessionId,
        input: params.input,
        conversation_id: params.conversationId,
        platform: params.platform,
        active_note: params.activeNote,
        selection: params.selection,
        ui_language: params.uiLanguage,
      }))
    })
  }, [])

  /** Cancel the current run. Equivalent to invoke('cancel_agent'). */
  const cancel = useCallback(() => {
    wsRef.current?.send(JSON.stringify({ type: 'cancel' }))
    doneResolversRef.current = []
  }, [])

  /** Confirm or deny a write-tool request. Equivalent to invoke('confirm_write_tool'). */
  const confirm = useCallback((approved: boolean) => {
    wsRef.current?.send(JSON.stringify({ type: 'confirm', approved }))
  }, [])

  return { on, run, cancel, confirm, wsRef }
}
