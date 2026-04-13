/**
 * Shared transcription WebSocket client with auto-reconnect.
 *
 * Protocol:
 *   client → text:   {"type":"start","language":"zh-TW"}
 *   client → binary: Int16Array (16 kHz, mono, little-endian) — continuous frames
 *   client → text:   {"type":"stop"}
 *
 *   server → text:   {"event":"whisper:done","data":{"text":"...","index":N}}
 *   server → text:   {"event":"whisper:flush_done"}
 *   server → text:   {"event":"whisper:error","data":"..."}
 */

export interface TranscribeSession {
  /** Send a PCM16 frame (Int16Array.buffer). */
  sendPcm(buffer: ArrayBuffer): void
  /** Tell server to flush remaining buffer; waits for flush_done internally. */
  stop(): void
  /** Permanently close the session (no reconnect). */
  close(): void
}

export function getTranscribeWsUrl(): string {
  const mobileBase = localStorage.getItem('mobile_base_url')
  const token = mobileBase
    ? (localStorage.getItem('mobile_token') ?? '')
    : (localStorage.getItem('daemon_token') ?? '')
  const base = mobileBase
    ? mobileBase.replace(/^https:/, 'wss:').replace(/^http:/, 'ws:')
    : 'ws://127.0.0.1:7787'
  return `${base}/api/v1/ws/transcribe${token ? `?token=${encodeURIComponent(token)}` : ''}`
}

const RECONNECT_DELAYS_MS = [500, 1000, 2000, 4000, 8000] // exponential backoff, capped at 8 s
const MAX_RECONNECT_ATTEMPTS = 5

/**
 * Open a transcription WebSocket session with automatic reconnection.
 *
 * On unexpected disconnect (not triggered by close()), the client reconnects
 * with exponential backoff and re-sends "start" so the server restores state.
 * In-flight PCM frames queued during reconnect are sent after re-connect.
 */
export function createTranscribeSession(
  language: string,
  onResult: (text: string, index: number) => void,
  onFlushDone: () => void,
  onError: (msg: string) => void,
): Promise<TranscribeSession> {
  return new Promise((resolve, reject) => {
    let ws: WebSocket | null = null
    let closed = false          // set by close() — no further reconnects
    let stopping = false        // set by stop() — waiting for flush_done
    let reconnectAttempts = 0
    // PCM frames buffered while reconnecting
    const pendingPcm: ArrayBuffer[] = []

    function connect() {
      const url = getTranscribeWsUrl()
      ws = new WebSocket(url)
      ws.binaryType = 'arraybuffer'

      ws.onopen = () => {
        reconnectAttempts = 0
        ws!.send(JSON.stringify({ type: 'start', language }))
        // Flush any frames buffered during reconnect
        for (const buf of pendingPcm) {
          ws!.send(buf)
        }
        pendingPcm.length = 0
      }

      ws.onmessage = (e) => {
        if (typeof e.data !== 'string') return
        try {
          const msg = JSON.parse(e.data as string) as { event: string; data?: unknown }
          if (msg.event === 'whisper:done') {
            const d = msg.data as { text: string; index: number }
            onResult(d.text ?? '', d.index ?? 0)
          } else if (msg.event === 'whisper:flush_done') {
            stopping = false
            onFlushDone()
          } else if (msg.event === 'whisper:error') {
            onError(String(msg.data ?? '轉錄失敗'))
          }
        } catch { /* ignore parse errors */ }
      }

      ws.onerror = () => {
        // onerror always followed by onclose — handle retry there
      }

      ws.onclose = (e) => {
        if (closed) return  // intentional close, no reconnect

        if (e.wasClean) {
          // Server sent a proper Close frame (e.g. server shutdown signal)
          onError('轉錄連線已關閉')
          return
        }

        // Unexpected disconnect — retry with backoff
        if (reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
          onError(`轉錄連線中斷，已重試 ${MAX_RECONNECT_ATTEMPTS} 次`)
          return
        }

        const delay = RECONNECT_DELAYS_MS[Math.min(reconnectAttempts, RECONNECT_DELAYS_MS.length - 1)]
        reconnectAttempts++
        setTimeout(connect, delay)
      }
    }

    // First connection — reject the promise if it fails immediately
    const url = getTranscribeWsUrl()
    const firstWs = new WebSocket(url)
    firstWs.binaryType = 'arraybuffer'

    firstWs.onopen = () => {
      ws = firstWs
      reconnectAttempts = 0
      ws.send(JSON.stringify({ type: 'start', language }))

      ws.onmessage = (e) => {
        if (typeof e.data !== 'string') return
        try {
          const msg = JSON.parse(e.data as string) as { event: string; data?: unknown }
          if (msg.event === 'whisper:done') {
            const d = msg.data as { text: string; index: number }
            onResult(d.text ?? '', d.index ?? 0)
          } else if (msg.event === 'whisper:flush_done') {
            stopping = false
            onFlushDone()
          } else if (msg.event === 'whisper:error') {
            onError(String(msg.data ?? '轉錄失敗'))
          }
        } catch { /* ignore */ }
      }

      ws.onerror = () => {}

      ws.onclose = (e) => {
        if (closed) return
        if (e.wasClean) { onError('轉錄連線已關閉'); return }
        if (reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
          onError(`轉錄連線中斷，已重試 ${MAX_RECONNECT_ATTEMPTS} 次`)
          return
        }
        const delay = RECONNECT_DELAYS_MS[Math.min(reconnectAttempts, RECONNECT_DELAYS_MS.length - 1)]
        reconnectAttempts++
        setTimeout(connect, delay)
      }

      resolve({
        sendPcm(buffer: ArrayBuffer) {
          if (closed) return
          if (ws && ws.readyState === WebSocket.OPEN) {
            ws.send(buffer)
          } else {
            // Buffer frames while reconnecting (cap at 5s worth ≈ 160 frames × 480 samples)
            if (pendingPcm.length < 160) pendingPcm.push(buffer)
          }
        },
        stop() {
          if (closed || stopping) return
          stopping = true
          if (ws && ws.readyState === WebSocket.OPEN) {
            ws.send(JSON.stringify({ type: 'stop' }))
          } else {
            // Not connected — flush_done will never come; resolve immediately
            stopping = false
            onFlushDone()
          }
        },
        close() {
          closed = true
          pendingPcm.length = 0
          ws?.close()
          ws = null
        },
      })
    }

    firstWs.onerror = () => reject(new Error('無法連接轉錄服務'))

    firstWs.onclose = (e) => {
      if (!e.wasClean) reject(new Error('無法連接轉錄服務'))
    }
  })
}
