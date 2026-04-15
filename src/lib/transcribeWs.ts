/**
 * Shared transcription WebSocket client — app-scoped singleton.
 *
 * The WebSocket connection is opened once on module load and never closed.
 * Each recording session just sends start/stop messages on the same connection.
 * close() on a session only clears its callbacks; the socket stays alive.
 *
 * Protocol:
 *   client → text:   {"type":"start","language":"zh-TW","vault_id":"..."}
 *   client → binary: Int16Array (16 kHz, mono, little-endian) — continuous frames
 *   client → text:   {"type":"stop"}
 *   client → text:   {"type":"rename_speaker","speaker":"SPEAKER_00","name":"Alice"}
 *
 *   server → text:   {"event":"meeting:started","data":{"meeting_id":"..."}}
 *   server → text:   {"event":"whisper:done","data":{"text":"...","index":N,"ts_ms":N,"speaker":"..."}}
 *   server → text:   {"event":"meeting:done","data":{"meeting_id":"..."}}
 *   server → text:   {"event":"whisper:error","data":"..."}
 */

export interface TranscribeSession {
  /** Send a PCM16 frame (Int16Array.buffer). */
  sendPcm(buffer: ArrayBuffer): void
  /** Tell server to flush remaining buffer. */
  stop(): void
  /** Rename a speaker label to a real name. */
  renameSpeaker(speaker: string, name: string): void
  /** End this session. Does NOT close the underlying WebSocket. */
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

// ── App-scoped singleton ───────────────────────────────────────────────────────

type SessionCallbacks = {
  onResult:        (text: string, index: number, speaker?: string, tsMs?: number) => void
  onFlushDone:     () => void
  onError:         (msg: string) => void
  onMeetingStarted?: (meetingId: string) => void
  onMeetingDone?:    (meetingId: string) => void
}

let _ws: WebSocket | null = null
let _reconnectTimer: ReturnType<typeof setTimeout> | null = null
let _callbacks: SessionCallbacks | null = null
/** start message buffered while WS is CONNECTING */
let _pendingStart: string | null = null

function dispatchMessage(msg: { event: string; data?: unknown }) {
  if (!_callbacks) return
  const cb = _callbacks
  if (msg.event === 'whisper:done') {
    const d = msg.data as { text: string; index: number; speaker?: string; ts_ms?: number }
    cb.onResult(d.text ?? '', d.index ?? 0, d.speaker, d.ts_ms)
  } else if (msg.event === 'meeting:done') {
    cb.onFlushDone()
    const d = msg.data as { meeting_id: string }
    cb.onMeetingDone?.(d.meeting_id)
  } else if (msg.event === 'whisper:flush_done') {
    cb.onFlushDone()
  } else if (msg.event === 'whisper:error') {
    cb.onError(String(msg.data ?? '轉錄失敗'))
  } else if (msg.event === 'meeting:started') {
    const d = msg.data as { meeting_id: string }
    cb.onMeetingStarted?.(d.meeting_id)
  }
}

function connectTranscribe() {
  if (_reconnectTimer) { clearTimeout(_reconnectTimer); _reconnectTimer = null }
  const sock = new WebSocket(getTranscribeWsUrl())
  sock.binaryType = 'arraybuffer'
  _ws = sock

  sock.onopen = () => {
    if (_pendingStart) {
      sock.send(_pendingStart)
      _pendingStart = null
    }
  }

  sock.onmessage = (e: MessageEvent) => {
    if (typeof e.data !== 'string') return
    try { dispatchMessage(JSON.parse(e.data) as { event: string; data?: unknown }) }
    catch { /* ignore parse errors */ }
  }

  sock.onclose = () => {
    _reconnectTimer = setTimeout(connectTranscribe, 3000)
  }

  sock.onerror = () => {}
}

// Connect immediately on module load
connectTranscribe()

// ── Public API ────────────────────────────────────────────────────────────────

/**
 * Begin a transcription session. Returns immediately (no await needed) since
 * the underlying WebSocket is already open.
 *
 * The previous session's callbacks are replaced; only one active session at a time.
 */
export function createTranscribeSession(
  language: string,
  onResult: (text: string, index: number, speaker?: string, tsMs?: number) => void,
  onFlushDone: () => void,
  onError: (msg: string) => void,
  options?: {
    vaultId?: string
    topic?: string
    parentMeetingId?: string
    onMeetingStarted?: (meetingId: string) => void
    onMeetingDone?: (meetingId: string) => void
  },
): Promise<TranscribeSession> {
  _callbacks = {
    onResult,
    onFlushDone,
    onError,
    onMeetingStarted: options?.onMeetingStarted,
    onMeetingDone:    options?.onMeetingDone,
  }

  const startMsg = JSON.stringify({
    type: 'start',
    language,
    vault_id:          options?.vaultId ?? '',
    topic:             options?.topic,
    parent_meeting_id: options?.parentMeetingId,
  })

  if (_ws && _ws.readyState === WebSocket.OPEN) {
    _ws.send(startMsg)
    _pendingStart = null
  } else {
    // WS is reconnecting — buffer the start message
    _pendingStart = startMsg
  }

  const session: TranscribeSession = {
    sendPcm(buffer: ArrayBuffer) {
      if (_ws && _ws.readyState === WebSocket.OPEN) {
        _ws.send(buffer)
      }
    },
    stop() {
      if (_ws && _ws.readyState === WebSocket.OPEN) {
        _ws.send(JSON.stringify({ type: 'stop' }))
      } else {
        // WS not available — resolve immediately so caller doesn't hang
        onFlushDone()
      }
    },
    renameSpeaker(speaker: string, name: string) {
      if (_ws && _ws.readyState === WebSocket.OPEN) {
        _ws.send(JSON.stringify({ type: 'rename_speaker', speaker, name }))
      }
    },
    close() {
      // Clear callbacks only; do NOT close the shared WebSocket
      if (_callbacks?.onFlushDone === onFlushDone) _callbacks = null
      _pendingStart = null
    },
  }

  return Promise.resolve(session)
}
