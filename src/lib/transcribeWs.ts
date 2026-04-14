/**
 * Shared transcription WebSocket client with auto-reconnect.
 *
 * Protocol:
 *   client → text:   {"type":"start","language":"zh-TW","vault_id":"..."}
 *   client → binary: Int16Array (16 kHz, mono, little-endian) — continuous frames
 *   client → text:   {"type":"stop"}
 *   client → text:   {"type":"rename_speaker","speaker":"SPEAKER_00","name":"Alice"}
 *
 *   server → text:   {"event":"meeting:started","data":{"meeting_id":"..."}}
 *   server → text:   {"event":"whisper:done","data":{"text":"...","index":N,"ts_ms":N,"speaker":"..."}}
 *   server → text:   {"event":"whisper:flush_done"}
 *   server → text:   {"event":"whisper:error","data":"..."}
 */

export interface TranscribeSession {
  /** Send a PCM16 frame (Int16Array.buffer). */
  sendPcm(buffer: ArrayBuffer): void
  /** Tell server to flush remaining buffer; waits for flush_done internally. */
  stop(): void
  /** Rename a speaker label to a real name (sent to server). */
  renameSpeaker(speaker: string, name: string): void
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
  onResult: (text: string, index: number, speaker?: string, tsMs?: number) => void,
  onFlushDone: () => void,
  onError: (msg: string) => void,
  options?: {
    vaultId?: string
    onMeetingStarted?: (meetingId: string) => void
    onMeetingDone?: (meetingId: string) => void
  },
): Promise<TranscribeSession> {
  const vaultId = options?.vaultId ?? ''
  const onMeetingStarted = options?.onMeetingStarted

  function handleMessage(e: MessageEvent, onMsg: (msg: { event: string; data?: unknown }) => void) {
    if (typeof e.data !== 'string') return
    try {
      onMsg(JSON.parse(e.data as string) as { event: string; data?: unknown })
    } catch { /* ignore parse errors */ }
  }

  function dispatch(msg: { event: string; data?: unknown }) {
    if (msg.event === 'whisper:done') {
      const d = msg.data as { text: string; index: number; speaker?: string; ts_ms?: number }
      onResult(d.text ?? '', d.index ?? 0, d.speaker, d.ts_ms)
    } else if (msg.event === 'whisper:flush_done' || msg.event === 'meeting:done') {
      // meeting:done is sent after SpeakerEngine attribution completes (two-phase shutdown).
      // whisper:flush_done kept for compatibility.
      stopping = false
      onFlushDone()
      if (msg.event === 'meeting:done') {
        const d = msg.data as { meeting_id: string }
        options?.onMeetingDone?.(d.meeting_id)
      }
    } else if (msg.event === 'whisper:error') {
      onError(String(msg.data ?? '轉錄失敗'))
    } else if (msg.event === 'meeting:started') {
      const d = msg.data as { meeting_id: string }
      onMeetingStarted?.(d.meeting_id)
    }
  }

  // These are captured by dispatch / closures below
  let stopping = false

  return new Promise((resolve, reject) => {
    let ws: WebSocket | null = null
    let closed = false          // set by close() — no further reconnects
    stopping = false
    let reconnectAttempts = 0
    // PCM frames buffered while reconnecting
    const pendingPcm: ArrayBuffer[] = []

    function connect() {
      const url = getTranscribeWsUrl()
      ws = new WebSocket(url)
      ws.binaryType = 'arraybuffer'

      ws.onopen = () => {
        reconnectAttempts = 0
        ws!.send(JSON.stringify({ type: 'start', language, vault_id: vaultId }))
        // Flush any frames buffered during reconnect
        for (const buf of pendingPcm) {
          ws!.send(buf)
        }
        pendingPcm.length = 0
      }

      ws.onmessage = (e) => handleMessage(e, dispatch)
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
    }

    // First connection — reject the promise if it fails immediately
    const url = getTranscribeWsUrl()
    const firstWs = new WebSocket(url)
    firstWs.binaryType = 'arraybuffer'

    firstWs.onopen = () => {
      ws = firstWs
      reconnectAttempts = 0
      ws.send(JSON.stringify({ type: 'start', language, vault_id: vaultId }))
      ws.onmessage = (e) => handleMessage(e, dispatch)
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
            if (pendingPcm.length < 160) pendingPcm.push(buffer)
          }
        },
        stop() {
          if (closed || stopping) return
          stopping = true
          if (ws && ws.readyState === WebSocket.OPEN) {
            ws.send(JSON.stringify({ type: 'stop' }))
          } else {
            stopping = false
            onFlushDone()
          }
        },
        renameSpeaker(speaker: string, name: string) {
          if (closed) return
          if (ws && ws.readyState === WebSocket.OPEN) {
            ws.send(JSON.stringify({ type: 'rename_speaker', speaker, name }))
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
