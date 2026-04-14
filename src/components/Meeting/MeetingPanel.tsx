import { useState, useRef, useCallback, useEffect } from 'react'
import { createTranscribeSession, TranscribeSession } from '../../lib/transcribeWs'
import { api } from '../../lib/api'
import type { MeetingSummary, MeetingSegment } from '../../lib/api'
import { useSettingsStore } from '../../stores/settingsStore'

// ─── Audio constants (same as useVoiceRecorder) ──────────────────────────────
const WHISPER_SAMPLE_RATE = 16_000
const WORKLET_URL = new URL('../../worklets/voice-processor.js', import.meta.url).href

// ─── Segment type ─────────────────────────────────────────────────────────────
interface LiveSegment {
  index: number
  speaker?: string
  text: string
  tsMs: number
}

// ─── Speaker color palette ───────────────────────────────────────────────────
const SPEAKER_COLORS = [
  '#60a5fa', '#34d399', '#f472b6', '#fb923c', '#a78bfa',
  '#facc15', '#2dd4bf', '#f87171', '#94a3b8', '#86efac',
]
function speakerColor(speaker: string): string {
  let h = 0
  for (let i = 0; i < speaker.length; i++) h = (h * 31 + speaker.charCodeAt(i)) >>> 0
  return SPEAKER_COLORS[h % SPEAKER_COLORS.length]
}

function formatTs(tsMs: number): string {
  const m = Math.floor(tsMs / 60_000)
  const s = Math.floor((tsMs % 60_000) / 1000)
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`
}

// ─── MeetingPanel ─────────────────────────────────────────────────────────────
export default function MeetingPanel() {
  const [language, setLanguage] = useState('auto')
  const vaultId = useRef<string>('')
  const { settings, loadSystem } = useSettingsStore()
  const diarizePath = settings.diarize_model_path ?? ''

  useEffect(() => { loadSystem() }, [loadSystem])

  // resolve vault id on mount
  useEffect(() => {
    import('@tauri-apps/api/core').then(({ invoke }) =>
      invoke<string>('get_vault_uuid').then(id => { vaultId.current = id }).catch(() => {})
    ).catch(() => {})
  }, [])

  // ─── Recording state ────────────────────────────────────────────────────────
  type Mode = 'idle' | 'recording' | 'processing' | 'done' | 'error'
  const [mode, setMode] = useState<Mode>('idle')
  const [error, setError] = useState('')
  const [meetingId, setMeetingId] = useState<string | null>(null)
  const [duration, setDuration] = useState(0)           // seconds
  const [segments, setSegments] = useState<LiveSegment[]>([])
  const [speakerNames, setSpeakerNames] = useState<Record<string, string>>({})

  // Speaker rename UI
  const [editingSpeaker, setEditingSpeaker] = useState<string | null>(null)
  const [editName, setEditName] = useState('')

  // History panel
  const [showHistory, setShowHistory] = useState(false)
  const [history, setHistory] = useState<MeetingSummary[]>([])
  const [historyLoading, setHistoryLoading] = useState(false)

  // Session refs
  const sessionRef = useRef<TranscribeSession | null>(null)
  const audioCtxRef = useRef<AudioContext | null>(null)
  const workletRef = useRef<AudioWorkletNode | null>(null)
  const streamRef = useRef<MediaStream | null>(null)
  const durationTimerRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const startTimeRef = useRef<number>(0)

  const scrollRef = useRef<HTMLDivElement>(null)

  // auto-scroll on new segments
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [segments])

  // ─── Load history ────────────────────────────────────────────────────────────
  const loadHistory = useCallback(async () => {
    setHistoryLoading(true)
    try {
      const res = await api.listMeetings(vaultId.current || undefined)
      setHistory(res.meetings)
    } catch { /* ignore */ } finally {
      setHistoryLoading(false)
    }
  }, [])

  useEffect(() => {
    if (showHistory) loadHistory()
  }, [showHistory, loadHistory])

  // ─── Start recording ─────────────────────────────────────────────────────────
  const startRecording = useCallback(async () => {
    setError('')
    setSegments([])
    setSpeakerNames({})
    setMeetingId(null)
    setDuration(0)

    try {
      const session = await createTranscribeSession(
        language,
        (text, index, _speaker, tsMs) => {
          // speaker is omitted here — SpeakerEngine assigns speakers asynchronously.
          // We display text immediately without a label; labels are filled in on meeting:done.
          if (!text.trim()) return
          setSegments(prev => [...prev, { index, text, tsMs: tsMs ?? 0 }])
        },
        () => {
          // whisper:flush_done (legacy) — Whisper tasks flushed
          setMode('processing')
        },
        (msg) => {
          setError(msg)
          setMode('error')
        },
        {
          vaultId: vaultId.current,
          onMeetingStarted: (mid) => {
            setMeetingId(mid)
          },
          onMeetingDone: async (mid) => {
            // SpeakerEngine has completed — reload segments with speaker attribution
            try {
              const result = await api.getMeeting(mid)
              setSegments(result.segments.map((s: MeetingSegment) => ({
                index: s.seg_index,
                speaker: s.speaker ?? undefined,
                text: s.text,
                tsMs: s.ts_ms,
              })))
            } catch { /* ignore — segments already displayed without speakers */ }
            setMode('done')
          },
        },
      )
      sessionRef.current = session

      // ── Audio capture ────────────────────────────────────────────────────────
      const ctx = new AudioContext({ sampleRate: WHISPER_SAMPLE_RATE })
      await ctx.audioWorklet.addModule(WORKLET_URL)
      if (ctx.state === 'suspended') await ctx.resume()

      const stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: false })
      const source = ctx.createMediaStreamSource(stream)
      const worklet = new AudioWorkletNode(ctx, 'voice-processor')
      const silentGain = ctx.createGain()
      silentGain.gain.value = 0
      worklet.connect(silentGain)
      silentGain.connect(ctx.destination)

      worklet.port.onmessage = (e: MessageEvent<{ samples: Float32Array }>) => {
        const frame = e.data.samples
        // Convert Float32 → Int16
        const pcm16 = new Int16Array(frame.length)
        for (let i = 0; i < frame.length; i++) {
          pcm16[i] = Math.max(-32768, Math.min(32767, Math.round(frame[i] * 32768)))
        }
        session.sendPcm(pcm16.buffer)
      }

      source.connect(worklet)

      audioCtxRef.current = ctx
      workletRef.current = worklet
      streamRef.current = stream

      // Duration timer
      startTimeRef.current = Date.now()
      durationTimerRef.current = setInterval(() => {
        setDuration(Math.floor((Date.now() - startTimeRef.current) / 1000))
      }, 1000)

      setMode('recording')
    } catch (e) {
      setError(String(e))
      setMode('error')
    }
  }, [language])

  // ─── Stop recording ──────────────────────────────────────────────────────────
  const stopRecording = useCallback(() => {
    if (durationTimerRef.current) {
      clearInterval(durationTimerRef.current)
      durationTimerRef.current = null
    }
    // Stop audio pipeline
    workletRef.current?.port.postMessage({ type: 'stop' })
    streamRef.current?.getTracks().forEach(t => t.stop())
    audioCtxRef.current?.close()
    workletRef.current = null
    streamRef.current = null
    audioCtxRef.current = null

    // Tell server to flush; show 'processing' immediately so the user knows
    // the meeting stopped (SpeakerEngine drain may take up to 30s before 'done').
    sessionRef.current?.stop()
    setMode('processing')
  }, [])

  // ─── Rename speaker ──────────────────────────────────────────────────────────
  const commitRename = useCallback(() => {
    if (!editingSpeaker || !editName.trim()) { setEditingSpeaker(null); return }
    const name = editName.trim()
    setSpeakerNames(prev => ({ ...prev, [editingSpeaker]: name }))
    sessionRef.current?.renameSpeaker(editingSpeaker, name)
    // Also update via REST if we have the meeting id and it's finalized
    if (meetingId && mode !== 'recording') {
      api.renameMeetingSpeaker(meetingId, editingSpeaker, name).catch(() => {})
    }
    setEditingSpeaker(null)
    setEditName('')
  }, [editingSpeaker, editName, meetingId, mode])

  // ─── Cleanup on unmount ──────────────────────────────────────────────────────
  useEffect(() => {
    return () => {
      if (durationTimerRef.current) clearInterval(durationTimerRef.current)
      sessionRef.current?.close()
      streamRef.current?.getTracks().forEach(t => t.stop())
      audioCtxRef.current?.close()
    }
  }, [])

  // ─── Resolve display name ────────────────────────────────────────────────────
  const displayName = (speaker?: string) =>
    speaker ? (speakerNames[speaker] ?? speaker) : '未知'

  // ─── Group consecutive segments by speaker ───────────────────────────────────
  type Group = { speaker?: string; segs: LiveSegment[] }
  const groups: Group[] = []
  for (const seg of segments) {
    const last = groups[groups.length - 1]
    if (last && last.speaker === seg.speaker) {
      last.segs.push(seg)
    } else {
      groups.push({ speaker: seg.speaker, segs: [seg] })
    }
  }

  const durationStr = `${String(Math.floor(duration / 60)).padStart(2, '0')}:${String(duration % 60).padStart(2, '0')}`

  // ─── Panel styles ─────────────────────────────────────────────────────────────
  const panelStyle: React.CSSProperties = {
    display: 'flex', flexDirection: 'column', height: '100%',
    background: '#0f1117', color: '#e2e8f0', fontFamily: 'inherit',
    overflow: 'hidden',
  }

  // ─── History view ─────────────────────────────────────────────────────────────
  if (showHistory) {
    return (
      <div style={panelStyle}>
        <div style={{ padding: '12px 16px', borderBottom: '1px solid #1e2433', display: 'flex', alignItems: 'center', gap: 8 }}>
          <button
            onClick={() => setShowHistory(false)}
            style={{ background: 'none', border: 'none', color: '#94a3b8', cursor: 'pointer', fontSize: 18, padding: 0 }}
          >←</button>
          <span style={{ fontWeight: 600, fontSize: 15 }}>會議記錄</span>
        </div>
        <div style={{ flex: 1, overflowY: 'auto', padding: 12 }}>
          {historyLoading && <div style={{ color: '#64748b', textAlign: 'center', padding: 24 }}>載入中…</div>}
          {!historyLoading && history.length === 0 && (
            <div style={{ color: '#64748b', textAlign: 'center', padding: 24 }}>尚無會議記錄</div>
          )}
          {history.map(m => (
            <MeetingHistoryCard key={m.meeting_id} meeting={m} onDelete={async () => {
              await api.deleteMeeting(m.meeting_id)
              loadHistory()
            }} />
          ))}
        </div>
      </div>
    )
  }

  // ─── Main recording view ──────────────────────────────────────────────────────
  return (
    <div style={panelStyle}>
      {/* Header */}
      <div style={{ padding: '12px 16px', borderBottom: '1px solid #1e2433', display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
          <span style={{ fontSize: 18 }}>🎙</span>
          <span style={{ fontWeight: 600, fontSize: 15 }}>會議錄音</span>
          {mode === 'recording' && (
            <span style={{ fontSize: 13, color: '#94a3b8', fontVariantNumeric: 'tabular-nums' }}>{durationStr}</span>
          )}
          {mode === 'recording' && (
            <span style={{ width: 8, height: 8, borderRadius: '50%', background: '#f87171', display: 'inline-block', animation: 'pulse 1s infinite' }} />
          )}
        </div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <select
            value={language}
            onChange={e => setLanguage(e.target.value)}
            disabled={mode === 'recording'}
            style={{ background: '#1e2433', border: '1px solid #2d3748', borderRadius: 4, color: '#94a3b8', cursor: 'pointer', fontSize: 12, padding: '3px 6px' }}
          >
            <option value="auto">自動</option>
            <option value="zh">中文</option>
            <option value="zh-TW">繁中</option>
            <option value="en">English</option>
            <option value="ja">日本語</option>
          </select>
          <button
            onClick={() => setShowHistory(true)}
            style={{ background: 'none', border: '1px solid #2d3748', borderRadius: 6, color: '#94a3b8', cursor: 'pointer', fontSize: 12, padding: '4px 10px' }}
          >
            歷史記錄
          </button>
        </div>
      </div>

      {/* Transcript */}
      <div ref={scrollRef} style={{ flex: 1, overflowY: 'auto', padding: '12px 16px' }}>
        {groups.length === 0 && mode === 'idle' && !diarizePath && (
          <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', marginTop: 48, gap: 12 }}>
            <span style={{ fontSize: 32 }}>🎤</span>
            <div style={{ color: '#f59e0b', fontSize: 14, fontWeight: 600 }}>需要設定說話者識別模型</div>
            <div style={{ color: '#64748b', fontSize: 13, textAlign: 'center', lineHeight: 1.7, maxWidth: 320 }}>
              會議錄音需要 CAM++ ONNX 模型來區分不同說話者。<br />
              請至<strong style={{ color: '#94a3b8' }}>系統設定 → 語音辨識 → 說話者識別</strong>下載並設定模型路徑。
            </div>
          </div>
        )}
        {groups.length === 0 && mode === 'idle' && diarizePath && (
          <div style={{ color: '#4a5568', textAlign: 'center', marginTop: 48, fontSize: 14 }}>
            按下開始錄音以記錄會議
          </div>
        )}
        {groups.length === 0 && mode === 'recording' && (
          <div style={{ color: '#4a5568', textAlign: 'center', marginTop: 48, fontSize: 14 }}>
            正在聆聽…
          </div>
        )}
        {groups.map((g, gi) => (
          <div key={gi} style={{ marginBottom: 14 }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 4 }}>
              {/* Speaker badge */}
              {editingSpeaker === g.speaker ? (
                <div style={{ display: 'flex', gap: 4 }}>
                  <input
                    autoFocus
                    value={editName}
                    onChange={e => setEditName(e.target.value)}
                    onKeyDown={e => { if (e.key === 'Enter') commitRename(); if (e.key === 'Escape') setEditingSpeaker(null) }}
                    style={{ background: '#1e2433', border: '1px solid #3b4d6a', borderRadius: 4, color: '#e2e8f0', fontSize: 12, padding: '2px 6px', width: 100 }}
                  />
                  <button onClick={commitRename} style={{ background: '#2563eb', border: 'none', borderRadius: 4, color: '#fff', cursor: 'pointer', fontSize: 11, padding: '2px 8px' }}>確定</button>
                  <button onClick={() => setEditingSpeaker(null)} style={{ background: '#374151', border: 'none', borderRadius: 4, color: '#9ca3af', cursor: 'pointer', fontSize: 11, padding: '2px 8px' }}>取消</button>
                </div>
              ) : (
                <button
                  onClick={() => { setEditingSpeaker(g.speaker ?? ''); setEditName(displayName(g.speaker)) }}
                  style={{
                    background: 'none', border: 'none', cursor: 'pointer', padding: 0,
                    fontSize: 12, fontWeight: 600,
                    color: g.speaker ? speakerColor(g.speaker) : '#64748b',
                  }}
                  title="點擊重命名"
                >
                  {displayName(g.speaker)}
                </button>
              )}
              <span style={{ fontSize: 11, color: '#4a5568', fontVariantNumeric: 'tabular-nums' }}>
                {formatTs(g.segs[0]?.tsMs ?? 0)}
              </span>
            </div>
            <div style={{ paddingLeft: 2, color: '#cbd5e1', lineHeight: 1.6, fontSize: 14 }}>
              {g.segs.map(s => s.text).join(' ')}
            </div>
          </div>
        ))}
        {mode === 'processing' && (
          <div style={{ color: '#64748b', fontSize: 13, marginTop: 8, textAlign: 'center' }}>
            AI 正在整理會議記錄…
          </div>
        )}
        {mode === 'done' && meetingId && (
          <div style={{ background: '#14532d22', border: '1px solid #166534', borderRadius: 8, padding: '10px 14px', marginTop: 8, fontSize: 13, color: '#86efac' }}>
            會議記錄已儲存，AI 摘要生成中（可在 vault/meetings/ 查看）
          </div>
        )}
      </div>

      {/* Error */}
      {error && (
        <div style={{ padding: '8px 16px', background: '#450a0a33', color: '#fca5a5', fontSize: 13 }}>
          {error}
        </div>
      )}

      {/* Controls */}
      <div style={{ padding: '12px 16px', borderTop: '1px solid #1e2433', display: 'flex', justifyContent: 'center', gap: 10 }}>
        {mode === 'idle' || mode === 'done' || mode === 'error' ? (
          <button
            onClick={startRecording}
            disabled={!diarizePath}
            title={!diarizePath ? '請先至系統設定設定 CAM++ 模型路徑' : undefined}
            style={{
              background: diarizePath ? '#dc2626' : '#374151',
              border: 'none', borderRadius: 24,
              color: diarizePath ? '#fff' : '#6b7280',
              cursor: diarizePath ? 'pointer' : 'not-allowed',
              fontSize: 14, fontWeight: 600, padding: '10px 28px',
              display: 'flex', alignItems: 'center', gap: 8,
              opacity: diarizePath ? 1 : 0.6,
            }}
          >
            <span style={{ fontSize: 16 }}>●</span> 開始錄音
          </button>
        ) : mode === 'recording' ? (
          <button
            onClick={stopRecording}
            style={{
              background: '#374151', border: 'none', borderRadius: 24, color: '#e2e8f0',
              cursor: 'pointer', fontSize: 14, fontWeight: 600, padding: '10px 28px',
              display: 'flex', alignItems: 'center', gap: 8,
            }}
          >
            <span style={{ fontSize: 16 }}>■</span> 停止錄音
          </button>
        ) : (
          <div style={{ color: '#64748b', fontSize: 13 }}>
            {mode === 'processing' ? '處理中…' : ''}
          </div>
        )}
      </div>

      <style>{`
        @keyframes pulse {
          0%, 100% { opacity: 1; }
          50% { opacity: 0.3; }
        }
      `}</style>
    </div>
  )
}

// ─── History card ─────────────────────────────────────────────────────────────
function MeetingHistoryCard({ meeting, onDelete }: { meeting: MeetingSummary; onDelete: () => void }) {
  const [expanded, setExpanded] = useState(false)
  const [detail, setDetail] = useState<{ segments: MeetingSegment[]; names: Record<string, string> } | null>(null)
  const [loading, setLoading] = useState(false)

  const load = async () => {
    if (detail || loading) return
    setLoading(true)
    try {
      const res = await api.getMeeting(meeting.meeting_id)
      const names: Record<string, string> = meeting.speaker_names_json
        ? JSON.parse(meeting.speaker_names_json)
        : {}
      setDetail({ segments: res.segments, names })
    } catch { /* ignore */ } finally {
      setLoading(false)
    }
  }

  const toggle = () => {
    if (!expanded) load()
    setExpanded(e => !e)
  }

  const dateStr = meeting.started_at
    ? new Date(meeting.started_at).toLocaleString('zh-TW', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
    : '—'

  const statusColor: Record<string, string> = {
    recording: '#facc15', done: '#4ade80', cancelled: '#6b7280', processing: '#60a5fa',
  }

  return (
    <div style={{ background: '#111827', border: '1px solid #1e2433', borderRadius: 8, marginBottom: 8, overflow: 'hidden' }}>
      <div
        onClick={toggle}
        style={{ padding: '10px 14px', cursor: 'pointer', display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}
      >
        <div>
          <div style={{ fontSize: 13, fontWeight: 500, color: '#e2e8f0' }}>{dateStr}</div>
          <div style={{ fontSize: 11, color: '#64748b', marginTop: 2 }}>
            {meeting.language ?? 'auto'} · {meeting.status && (
              <span style={{ color: statusColor[meeting.status] ?? '#94a3b8' }}>{meeting.status}</span>
            )}
            {meeting.note_path && <span style={{ color: '#6b7280' }}> · {meeting.note_path}</span>}
          </div>
        </div>
        <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
          <button
            onClick={e => { e.stopPropagation(); if (confirm('刪除此會議記錄？')) onDelete() }}
            style={{ background: 'none', border: 'none', color: '#6b7280', cursor: 'pointer', fontSize: 14, padding: '2px 4px' }}
            title="刪除"
          >✕</button>
          <span style={{ color: '#4a5568', fontSize: 12 }}>{expanded ? '▲' : '▼'}</span>
        </div>
      </div>

      {expanded && (
        <div style={{ borderTop: '1px solid #1e2433', padding: '10px 14px', maxHeight: 320, overflowY: 'auto' }}>
          {loading && <div style={{ color: '#64748b', fontSize: 13 }}>載入中…</div>}
          {detail && detail.segments.length === 0 && (
            <div style={{ color: '#4a5568', fontSize: 13 }}>無逐字稿片段</div>
          )}
          {detail && detail.segments.map((seg, i) => (
            <div key={i} style={{ marginBottom: 10 }}>
              <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginBottom: 2 }}>
                <span style={{ fontSize: 11, fontWeight: 600, color: seg.speaker ? speakerColor(seg.speaker) : '#64748b' }}>
                  {seg.speaker ? (detail.names[seg.speaker] ?? seg.speaker) : '未知'}
                </span>
                <span style={{ fontSize: 11, color: '#4a5568', fontVariantNumeric: 'tabular-nums' }}>{formatTs(seg.ts_ms)}</span>
              </div>
              <div style={{ fontSize: 13, color: '#cbd5e1', lineHeight: 1.5 }}>{seg.text}</div>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
