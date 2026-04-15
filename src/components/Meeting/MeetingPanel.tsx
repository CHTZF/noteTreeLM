import { useState, useRef, useCallback, useEffect } from 'react'
import { listen } from '@tauri-apps/api/event'
import { createTranscribeSession, TranscribeSession } from '../../lib/transcribeWs'
import { api } from '../../lib/api'
import type { MeetingSummary, MeetingSegment } from '../../lib/api'
import { useSettingsStore } from '../../stores/settingsStore'
import { useMicStore } from '../../stores/micStore'

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
  const meetingHolderIdRef = useRef(crypto.randomUUID())
  const micHolder = useMicStore(s => s.holder)
  const micBusy = micHolder !== null && micHolder !== meetingHolderIdRef.current
  const { settings, loadSystem } = useSettingsStore()
  const diarizePath = settings.diarize_model_path ?? ''

  useEffect(() => { loadSystem() }, [loadSystem])

  // resolve vault id on mount
  useEffect(() => {
    import('@tauri-apps/api/core').then(({ invoke }) =>
      invoke<string>('get_vault_uuid').then(id => { vaultId.current = id }).catch(() => {})
    ).catch(() => {})
  }, [])

  // ─── View state ──────────────────────────────────────────────────────────────
  // 'prep'      = 準備開會 entry screen
  // 'recording' = live recording
  // 'done'      = post-recording summary
  // 'history'   = history list
  type View = 'prep' | 'recording' | 'done' | 'history'
  const [view, setView] = useState<View>('prep')

  // ─── Prep state ──────────────────────────────────────────────────────────────
  type MeetingMode = 'new' | 'continue'
  const [meetingMode, setMeetingMode] = useState<MeetingMode>('new')
  const [topic, setTopic] = useState('')
  const [participants, setParticipants] = useState<string[]>([])
  const [brief, setBrief] = useState('')
  const [briefStreaming, setBriefStreaming] = useState(false)
  const [briefSessionId, setBriefSessionId] = useState<string | null>(null)
  const [searchLoading, setSearchLoading] = useState(false)
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  // Sidebar brief visible during recording (延續 mode)
  const [sidebarOpen, setSidebarOpen] = useState(true)
  const briefRef = useRef('')   // keep in sync for sidebar during recording

  // ─── Recording state ─────────────────────────────────────────────────────────
  type RecordMode = 'idle' | 'recording' | 'processing' | 'error'
  const [recMode, setRecMode] = useState<RecordMode>('idle')
  const [error, setError] = useState('')
  const [meetingId, setMeetingId] = useState<string | null>(null)
  const [duration, setDuration] = useState(0)
  const [segments, setSegments] = useState<LiveSegment[]>([])
  const [speakerNames, setSpeakerNames] = useState<Record<string, string>>({})

  // Post-recording summarization
  const [notePath, setNotePath] = useState<string | null>(null)
  const [summarizing, setSummarizing] = useState(false)

  // Speaker rename
  const [editingSpeaker, setEditingSpeaker] = useState<string | null>(null)
  const [editName, setEditName] = useState('')

  // History
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

  // ─── Auto-scroll ─────────────────────────────────────────────────────────────
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [segments])

  // ─── Listen for Agent summarization ─────────────────────────────────────────
  useEffect(() => {
    let cancelled = false
    let unlisten: (() => void) | null = null
    listen<{ meeting_id: string; note_path: string }>('meeting:summarized', (event) => {
      if (cancelled) return
      if (meetingId && event.payload.meeting_id === meetingId) {
        setNotePath(event.payload.note_path)
        setSummarizing(false)
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [meetingId])

  // ─── Debounced pre-brief search ──────────────────────────────────────────────
  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current)
    if (!topic.trim()) {
      setParticipants([])
      setBrief('')
      briefRef.current = ''
      setBriefSessionId(null)
      return
    }
    debounceRef.current = setTimeout(async () => {
      if (!topic.trim() || meetingMode !== 'continue') return
      setSearchLoading(true)
      setBrief('')
      briefRef.current = ''

      // 1) Load participants
      try {
        const res = await api.getMeetingParticipants(vaultId.current, topic)
        setParticipants(res.participants ?? [])
      } catch { /* ignore */ }

      // 2) Start streaming brief
      try {
        const { session_id } = await api.preMeetingBrief(vaultId.current, topic)
        setBriefSessionId(session_id)
        setBriefStreaming(true)
      } catch { /* ignore */ }
      setSearchLoading(false)
    }, 1000)

    return () => { if (debounceRef.current) clearTimeout(debounceRef.current) }
  }, [topic, meetingMode])

  // ─── Listen for brief streaming tokens ──────────────────────────────────────
  useEffect(() => {
    if (!briefSessionId) return
    let cancelled = false
    let unlistenToken: (() => void) | null = null
    let unlistenDone: (() => void) | null = null

    listen<{ t: string; session_id: string }>('llm:token', (event) => {
      if (cancelled) return
      if (event.payload.session_id !== briefSessionId) return
      briefRef.current += event.payload.t
      setBrief(briefRef.current)
    }).then(fn => { if (cancelled) fn(); else unlistenToken = fn })

    listen<{ t: string; session_id: string }>('llm:done', (event) => {
      if (cancelled) return
      if (event.payload.session_id !== briefSessionId) return
      if (event.payload.t) {
        briefRef.current = event.payload.t
        setBrief(event.payload.t)
      }
      setBriefStreaming(false)
      setBriefSessionId(null)
    }).then(fn => { if (cancelled) fn(); else unlistenDone = fn })

    return () => {
      cancelled = true
      unlistenToken?.()
      unlistenDone?.()
    }
  }, [briefSessionId])

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
    if (view === 'history') loadHistory()
  }, [view, loadHistory])

  // ─── Start recording ─────────────────────────────────────────────────────────
  const startRecording = useCallback(async () => {
    if (!useMicStore.getState().claim(meetingHolderIdRef.current)) {
      setError('麥克風已被其他功能使用中')
      return
    }
    setError('')
    setSegments([])
    setSpeakerNames({})
    setMeetingId(null)
    setDuration(0)
    setNotePath(null)
    setSummarizing(false)

    try {
      const session = await createTranscribeSession(
        language,
        (text, index, _speaker, tsMs) => {
          if (!text.trim()) return
          setSegments(prev => [...prev, { index, text, tsMs: tsMs ?? 0 }])
        },
        () => { setRecMode('processing') },
        (msg) => { setError(msg); setRecMode('error') },
        {
          vaultId: vaultId.current,
          topic: topic || undefined,
          parentMeetingId: meetingMode === 'continue' ? undefined : undefined, // TODO: wire parent_meeting_id
          onMeetingStarted: (mid) => { setMeetingId(mid) },
          onMeetingDone: async (mid) => {
            // Close session immediately to prevent reconnect → spurious empty meeting
            sessionRef.current?.close()
            sessionRef.current = null
            try {
              const result = await api.getMeeting(mid)
              setSegments(result.segments.map((s: MeetingSegment) => ({
                index: s.seg_index,
                speaker: s.speaker ?? undefined,
                text: s.text,
                tsMs: s.ts_ms,
              })))
              if (result.meeting?.note_path) {
                setNotePath(result.meeting.note_path)
              } else {
                setSummarizing(true)
              }
            } catch { /* ignore */ }
            setRecMode('idle')
            setView('done')
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

      startTimeRef.current = Date.now()
      durationTimerRef.current = setInterval(() => {
        setDuration(Math.floor((Date.now() - startTimeRef.current) / 1000))
      }, 1000)

      setView('recording')
      setRecMode('recording')
    } catch (e) {
      setError(String(e))
      setRecMode('error')
    }
  }, [language, topic, meetingMode])

  // ─── Stop recording ──────────────────────────────────────────────────────────
  const stopRecording = useCallback(() => {
    if (durationTimerRef.current) {
      clearInterval(durationTimerRef.current)
      durationTimerRef.current = null
    }
    workletRef.current?.port.postMessage({ type: 'stop' })
    streamRef.current?.getTracks().forEach(t => t.stop())
    audioCtxRef.current?.close()
    workletRef.current = null
    streamRef.current = null
    audioCtxRef.current = null
    sessionRef.current?.stop()
    setRecMode('processing')
    useMicStore.getState().release(meetingHolderIdRef.current)
  }, [])

  // ─── Rename speaker ──────────────────────────────────────────────────────────
  const commitRename = useCallback(() => {
    if (!editingSpeaker || !editName.trim()) { setEditingSpeaker(null); return }
    const name = editName.trim()
    setSpeakerNames(prev => ({ ...prev, [editingSpeaker]: name }))
    sessionRef.current?.renameSpeaker(editingSpeaker, name)
    if (meetingId && recMode !== 'recording') {
      api.renameMeetingSpeaker(meetingId, editingSpeaker, name).catch(() => {})
    }
    setEditingSpeaker(null)
    setEditName('')
  }, [editingSpeaker, editName, meetingId, recMode])

  // ─── Cleanup on unmount ──────────────────────────────────────────────────────
  useEffect(() => {
    return () => {
      if (durationTimerRef.current) clearInterval(durationTimerRef.current)
      sessionRef.current?.close()
      streamRef.current?.getTracks().forEach(t => t.stop())
      audioCtxRef.current?.close()
    }
  }, [])

  const displayName = (speaker?: string) =>
    speaker ? (speakerNames[speaker] ?? speaker) : '未知'

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

  const panelStyle: React.CSSProperties = {
    display: 'flex', flexDirection: 'column', height: '100%',
    background: '#0f1117', color: '#e2e8f0', fontFamily: 'inherit',
    overflow: 'hidden',
  }

  // ─── History view ─────────────────────────────────────────────────────────────
  if (view === 'history') {
    return (
      <div style={panelStyle}>
        <div style={{ padding: '12px 16px', borderBottom: '1px solid #1e2433', display: 'flex', alignItems: 'center', gap: 8 }}>
          <button
            onClick={() => setView('prep')}
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

  // ─── Done view ────────────────────────────────────────────────────────────────
  if (view === 'done') {
    return (
      <div style={panelStyle}>
        <div style={{ padding: '12px 16px', borderBottom: '1px solid #1e2433', display: 'flex', alignItems: 'center', gap: 8 }}>
          <button
            onClick={() => { setView('prep'); setBrief(''); briefRef.current = ''; setTopic(''); setParticipants([]) }}
            style={{ background: 'none', border: 'none', color: '#94a3b8', cursor: 'pointer', fontSize: 18, padding: 0 }}
          >←</button>
          <span style={{ fontWeight: 600, fontSize: 15 }}>會議結束</span>
        </div>
        <div ref={scrollRef} style={{ flex: 1, overflowY: 'auto', padding: '12px 16px' }}>
          {/* Summary status */}
          <div style={{ background: '#14532d22', border: '1px solid #166534', borderRadius: 8, padding: '10px 14px', marginBottom: 12, fontSize: 13 }}>
            {notePath ? (
              <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                <span style={{ color: '#86efac' }}>✓ 會議記錄已整理完成</span>
                <span style={{ color: '#4ade80', fontFamily: 'monospace', fontSize: 12 }}>{notePath}</span>
                <button
                  onClick={async () => {
                    if (!meetingId) return
                    setNotePath(null)
                    setSummarizing(true)
                    try { await api.summarizeMeeting(meetingId) } catch { setSummarizing(false) }
                  }}
                  style={{ background: 'none', border: '1px solid #166534', borderRadius: 4, color: '#86efac', cursor: 'pointer', fontSize: 11, padding: '2px 8px', marginLeft: 'auto' }}
                >重新整理</button>
              </div>
            ) : summarizing ? (
              <div style={{ display: 'flex', alignItems: 'center', gap: 8, color: '#60a5fa' }}>
                <span style={{ display: 'inline-block', animation: 'spin 1s linear infinite', fontSize: 14 }}>⟳</span>
                Agent 正在整理會議記錄…
              </div>
            ) : (
              <div style={{ color: '#64748b' }}>會議記錄已儲存</div>
            )}
          </div>

          {/* Transcript */}
          {groups.map((g, gi) => (
            <div key={gi} style={{ marginBottom: 14 }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 4 }}>
                {editingSpeaker === g.speaker ? (
                  <div style={{ display: 'flex', gap: 4 }}>
                    <input autoFocus value={editName} onChange={e => setEditName(e.target.value)}
                      onKeyDown={e => { if (e.key === 'Enter') commitRename(); if (e.key === 'Escape') setEditingSpeaker(null) }}
                      style={{ background: '#1e2433', border: '1px solid #3b4d6a', borderRadius: 4, color: '#e2e8f0', fontSize: 12, padding: '2px 6px', width: 100 }}
                    />
                    <button onClick={commitRename} style={{ background: '#2563eb', border: 'none', borderRadius: 4, color: '#fff', cursor: 'pointer', fontSize: 11, padding: '2px 8px' }}>確定</button>
                    <button onClick={() => setEditingSpeaker(null)} style={{ background: '#374151', border: 'none', borderRadius: 4, color: '#9ca3af', cursor: 'pointer', fontSize: 11, padding: '2px 8px' }}>取消</button>
                  </div>
                ) : (
                  <button onClick={() => { setEditingSpeaker(g.speaker ?? ''); setEditName(displayName(g.speaker)) }}
                    style={{ background: 'none', border: 'none', cursor: 'pointer', padding: 0, fontSize: 12, fontWeight: 600, color: g.speaker ? speakerColor(g.speaker) : '#64748b' }}
                    title="點擊重命名">{displayName(g.speaker)}</button>
                )}
                <span style={{ fontSize: 11, color: '#4a5568', fontVariantNumeric: 'tabular-nums' }}>{formatTs(g.segs[0]?.tsMs ?? 0)}</span>
              </div>
              <div style={{ paddingLeft: 2, color: '#cbd5e1', lineHeight: 1.6, fontSize: 14 }}>
                {g.segs.map(s => s.text).join(' ')}
              </div>
            </div>
          ))}
        </div>
        {error && <div style={{ padding: '8px 16px', background: '#450a0a33', color: '#fca5a5', fontSize: 13 }}>{error}</div>}
        <div style={{ padding: '12px 16px', borderTop: '1px solid #1e2433', display: 'flex', justifyContent: 'center' }}>
          <button
            onClick={() => { setView('prep'); setBrief(''); briefRef.current = ''; setTopic(''); setParticipants([]) }}
            style={{ background: '#1e2433', border: '1px solid #2d3748', borderRadius: 20, color: '#94a3b8', cursor: 'pointer', fontSize: 13, padding: '8px 24px' }}
          >新會議</button>
        </div>
        <style>{`@keyframes spin { from { transform: rotate(0deg); } to { transform: rotate(360deg); } }`}</style>
      </div>
    )
  }

  // ─── Recording view ───────────────────────────────────────────────────────────
  if (view === 'recording') {
    const hasBrief = meetingMode === 'continue' && brief.trim()
    return (
      <div style={{ ...panelStyle, flexDirection: 'row' }}>
        {/* Left sidebar: brief summary (only in 延續 mode) */}
        {hasBrief && sidebarOpen && (
          <div style={{
            width: 200, minWidth: 160, background: '#0a0d14', borderRight: '1px solid #1e2433',
            display: 'flex', flexDirection: 'column', overflow: 'hidden', flexShrink: 0,
          }}>
            <div style={{ padding: '8px 10px', borderBottom: '1px solid #1e2433', display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
              <span style={{ fontSize: 11, fontWeight: 600, color: '#60a5fa' }}>會前簡報</span>
              <button onClick={() => setSidebarOpen(false)}
                style={{ background: 'none', border: 'none', color: '#4a5568', cursor: 'pointer', fontSize: 12, padding: 0 }}>✕</button>
            </div>
            <div style={{ flex: 1, overflowY: 'auto', padding: '8px 10px', fontSize: 11, color: '#94a3b8', lineHeight: 1.6, whiteSpace: 'pre-wrap' }}>
              {brief}
            </div>
          </div>
        )}
        {hasBrief && !sidebarOpen && (
          <button
            onClick={() => setSidebarOpen(true)}
            style={{ writing: 'vertical-rl', background: '#0a0d14', border: 'none', borderRight: '1px solid #1e2433', color: '#60a5fa', cursor: 'pointer', fontSize: 11, padding: '8px 4px', flexShrink: 0 } as React.CSSProperties}
            title="顯示會前簡報"
          >◀ 簡報</button>
        )}

        {/* Main recording area */}
        <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
          {/* Header */}
          <div style={{ padding: '10px 14px', borderBottom: '1px solid #1e2433', display: 'flex', alignItems: 'center', gap: 10 }}>
            <span style={{ fontSize: 16 }}>🎙</span>
            {topic && <span style={{ fontSize: 13, color: '#e2e8f0', fontWeight: 500, maxWidth: 160, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{topic}</span>}
            <span style={{ fontSize: 13, color: '#94a3b8', fontVariantNumeric: 'tabular-nums' }}>{durationStr}</span>
            <span style={{ width: 8, height: 8, borderRadius: '50%', background: '#f87171', display: 'inline-block', animation: 'pulse 1s infinite' }} />
            {recMode === 'processing' && <span style={{ fontSize: 12, color: '#64748b' }}>處理中…</span>}
          </div>

          {/* Transcript */}
          <div ref={scrollRef} style={{ flex: 1, overflowY: 'auto', padding: '10px 14px' }}>
            {groups.length === 0 && recMode === 'recording' && (
              <div style={{ color: '#4a5568', textAlign: 'center', marginTop: 40, fontSize: 14 }}>正在聆聽…</div>
            )}
            {groups.map((g, gi) => (
              <div key={gi} style={{ marginBottom: 12 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 3 }}>
                  <button
                    onClick={() => { setEditingSpeaker(g.speaker ?? ''); setEditName(displayName(g.speaker)) }}
                    style={{ background: 'none', border: 'none', cursor: 'pointer', padding: 0, fontSize: 11, fontWeight: 600, color: g.speaker ? speakerColor(g.speaker) : '#64748b' }}
                  >{displayName(g.speaker)}</button>
                  <span style={{ fontSize: 11, color: '#4a5568', fontVariantNumeric: 'tabular-nums' }}>{formatTs(g.segs[0]?.tsMs ?? 0)}</span>
                </div>
                <div style={{ color: '#cbd5e1', lineHeight: 1.6, fontSize: 13 }}>
                  {g.segs.map(s => s.text).join(' ')}
                </div>
              </div>
            ))}
          </div>

          {/* Stop button */}
          <div style={{ padding: '10px 14px', borderTop: '1px solid #1e2433', display: 'flex', justifyContent: 'center' }}>
            {recMode === 'recording' && (
              <button
                onClick={stopRecording}
                style={{ background: '#374151', border: 'none', borderRadius: 24, color: '#e2e8f0', cursor: 'pointer', fontSize: 14, fontWeight: 600, padding: '10px 28px', display: 'flex', alignItems: 'center', gap: 8 }}
              >
                <span style={{ fontSize: 16 }}>■</span> 停止錄音
              </button>
            )}
          </div>
        </div>
        <style>{`@keyframes pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.3; } }`}</style>
      </div>
    )
  }

  // ─── Prep view (default) ─────────────────────────────────────────────────────
  return (
    <div style={panelStyle}>
      {/* Header */}
      <div style={{ padding: '12px 16px', borderBottom: '1px solid #1e2433', display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <span style={{ fontSize: 18 }}>🗓️</span>
          <span style={{ fontWeight: 600, fontSize: 15 }}>準備開會</span>
        </div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <select
            value={language}
            onChange={e => setLanguage(e.target.value)}
            style={{ background: '#1e2433', border: '1px solid #2d3748', borderRadius: 4, color: '#94a3b8', cursor: 'pointer', fontSize: 12, padding: '3px 6px' }}
          >
            <option value="auto">自動</option>
            <option value="zh">中文</option>
            <option value="zh-TW">繁中</option>
            <option value="en">English</option>
            <option value="ja">日本語</option>
          </select>
          <button
            onClick={() => setView('history')}
            style={{ background: 'none', border: '1px solid #2d3748', borderRadius: 6, color: '#94a3b8', cursor: 'pointer', fontSize: 12, padding: '4px 10px' }}
          >歷史記錄</button>
        </div>
      </div>

      <div style={{ flex: 1, overflowY: 'auto', padding: '16px' }}>
        {/* Mode toggle */}
        <div style={{ display: 'flex', gap: 8, marginBottom: 16 }}>
          {(['new', 'continue'] as MeetingMode[]).map(m => (
            <button
              key={m}
              onClick={() => {
                setMeetingMode(m)
                setBrief('')
                briefRef.current = ''
                setParticipants([])
              }}
              style={{
                flex: 1, padding: '7px 0', borderRadius: 8, fontSize: 13, fontWeight: 500,
                cursor: 'pointer', border: 'none', transition: 'all 0.15s',
                background: meetingMode === m ? '#2563eb' : '#1e2433',
                color: meetingMode === m ? '#fff' : '#94a3b8',
              }}
            >
              {m === 'new' ? '新會議' : '延續會議'}
            </button>
          ))}
        </div>

        {/* Topic input */}
        <div style={{ marginBottom: 14 }}>
          <div style={{ fontSize: 12, color: '#64748b', marginBottom: 6 }}>會議主題</div>
          <input
            value={topic}
            onChange={e => setTopic(e.target.value)}
            placeholder="例：infra 部署進度、Q3 產品規劃…"
            style={{
              width: '100%', background: '#1e2433', border: '1px solid #2d3748',
              borderRadius: 8, color: '#e2e8f0', fontSize: 14, padding: '9px 12px',
              outline: 'none', boxSizing: 'border-box',
            }}
          />
        </div>

        {/* Participants (auto-detected, 延續 mode only) */}
        {meetingMode === 'continue' && (
          <div style={{ marginBottom: 14 }}>
            <div style={{ fontSize: 12, color: '#64748b', marginBottom: 6 }}>
              {searchLoading ? '搜尋中…' : participants.length > 0 ? `找到 ${participants.length} 位相關人員` : '參與人員'}
            </div>
            {participants.length > 0 && (
              <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
                {participants.map(p => (
                  <span
                    key={p}
                    style={{
                      display: 'inline-flex', alignItems: 'center', gap: 4,
                      background: '#1e2d45', border: '1px solid #2563eb44',
                      borderRadius: 16, color: '#93c5fd', fontSize: 12, padding: '3px 10px',
                    }}
                  >
                    {p}
                    <button
                      onClick={() => setParticipants(prev => prev.filter(n => n !== p))}
                      style={{ background: 'none', border: 'none', color: '#60a5fa', cursor: 'pointer', fontSize: 11, padding: 0, lineHeight: 1 }}
                    >✕</button>
                  </span>
                ))}
              </div>
            )}
            {participants.length === 0 && !searchLoading && topic.trim() && (
              <div style={{ color: '#4a5568', fontSize: 12 }}>輸入主題後自動從歷史記錄找出相關人員</div>
            )}
          </div>
        )}

        {/* Streaming brief (延續 mode) */}
        {meetingMode === 'continue' && (brief || briefStreaming) && (
          <div style={{ marginBottom: 14 }}>
            <div style={{ fontSize: 12, color: '#64748b', marginBottom: 6, display: 'flex', alignItems: 'center', gap: 6 }}>
              會前簡報
              {briefStreaming && <span style={{ display: 'inline-block', animation: 'spin 1s linear infinite', fontSize: 12, color: '#60a5fa' }}>⟳</span>}
            </div>
            <div style={{
              background: '#111827', border: '1px solid #1e2433', borderRadius: 8,
              padding: '12px 14px', maxHeight: 280, overflowY: 'auto',
              fontSize: 13, color: '#cbd5e1', lineHeight: 1.7, whiteSpace: 'pre-wrap',
            }}>
              {brief}
              {briefStreaming && <span style={{ color: '#3b82f6', animation: 'blink 0.8s infinite' }}>▋</span>}
            </div>
          </div>
        )}

        {!diarizePath && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: 6, background: '#1c1100', border: '1px solid #92400e', borderRadius: 8, padding: '10px 14px', marginBottom: 14 }}>
            <div style={{ color: '#f59e0b', fontSize: 13, fontWeight: 600 }}>需要設定說話者識別模型</div>
            <div style={{ color: '#78716c', fontSize: 12, lineHeight: 1.6 }}>
              請至<strong style={{ color: '#94a3b8' }}>系統設定 → 語音辨識 → 說話者識別</strong>設定 CAM++ ONNX 模型路徑。
            </div>
          </div>
        )}
      </div>

      {/* Start recording button */}
      <div style={{ padding: '12px 16px', borderTop: '1px solid #1e2433', display: 'flex', justifyContent: 'center' }}>
        <button
          onClick={startRecording}
          disabled={!diarizePath || micBusy}
          title={!diarizePath ? '請先至系統設定設定 CAM++ 模型路徑' : micBusy ? '麥克風已被其他功能使用中' : undefined}
          style={{
            background: (diarizePath && !micBusy) ? '#dc2626' : '#374151',
            border: 'none', borderRadius: 24,
            color: (diarizePath && !micBusy) ? '#fff' : '#6b7280',
            cursor: (diarizePath && !micBusy) ? 'pointer' : 'not-allowed',
            fontSize: 14, fontWeight: 600, padding: '10px 32px',
            display: 'flex', alignItems: 'center', gap: 8,
            opacity: diarizePath ? 1 : 0.6,
          }}
        >
          <span style={{ fontSize: 16 }}>●</span> 開始錄音
        </button>
      </div>

      <style>{`
        @keyframes pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.3; } }
        @keyframes spin { from { transform: rotate(0deg); } to { transform: rotate(360deg); } }
        @keyframes blink { 0%, 100% { opacity: 1; } 50% { opacity: 0; } }
      `}</style>
    </div>
  )
}

// ─── History card ─────────────────────────────────────────────────────────────
function MeetingHistoryCard({ meeting, onDelete }: { meeting: MeetingSummary; onDelete: () => void }) {
  const [expanded, setExpanded] = useState(false)
  const [detail, setDetail] = useState<{ segments: MeetingSegment[]; names: Record<string, string> } | null>(null)
  const [loading, setLoading] = useState(false)
  const [summarizing, setSummarizing] = useState(false)
  const [notePath, setNotePath] = useState(meeting.note_path ?? null)

  useEffect(() => {
    let cancelled = false
    let unlisten: (() => void) | null = null
    listen<{ meeting_id: string; note_path: string }>('meeting:summarized', (event) => {
      if (cancelled) return
      if (event.payload.meeting_id === meeting.meeting_id) {
        setNotePath(event.payload.note_path)
        setSummarizing(false)
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [meeting.meeting_id])

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
      <div onClick={toggle} style={{ padding: '10px 14px', cursor: 'pointer', display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <div>
          <div style={{ fontSize: 13, fontWeight: 500, color: '#e2e8f0' }}>{dateStr}</div>
          <div style={{ fontSize: 11, color: '#64748b', marginTop: 2 }}>
            {meeting.language ?? 'auto'} · {meeting.status && (
              <span style={{ color: statusColor[meeting.status] ?? '#94a3b8' }}>{meeting.status}</span>
            )}
            {notePath
              ? <span style={{ color: '#4ade80' }}> · ✓ {notePath}</span>
              : summarizing
                ? <span style={{ color: '#60a5fa' }}> · ⟳ 整理中…</span>
                : <span
                    onClick={async (e) => {
                      e.stopPropagation()
                      setSummarizing(true)
                      try { await api.summarizeMeeting(meeting.meeting_id) } catch { setSummarizing(false) }
                    }}
                    style={{ color: '#3b82f6', cursor: 'pointer', marginLeft: 4 }}
                    title="觸發 Agent 整理"
                  > · 整理摘要</span>
            }
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
