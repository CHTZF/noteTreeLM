import { useEffect, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import type { VoiceState } from '../../hooks/useVoiceRecorder'

// ── 資料夾選擇器（inline 展開清單，手機友善）──────────────────────────────────
function FolderPicker({
  value,
  onChange,
  inputStyle,
}: {
  value: string
  onChange: (v: string) => void
  inputStyle: React.CSSProperties
}) {
  const [open, setOpen]       = useState(false)
  const [folders, setFolders] = useState<string[]>([])

  // 每次展開時重新載入（vault 可能有新資料夾）
  useEffect(() => {
    if (!open) return
    invoke<string[]>('list_folders')
      .then(list => setFolders(list.sort()))
      .catch(() => {})
  }, [open])

  // '' = 根目錄
  const options = ['', ...folders]

  return (
    <div>
      {/* trigger — 樣式與 input 一致 */}
      <button
        onClick={() => setOpen(o => !o)}
        style={{
          ...inputStyle,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          cursor: 'pointer',
          textAlign: 'left',
          color: value ? 'rgba(255,255,255,0.9)' : 'rgba(255,255,255,0.35)',
        }}
      >
        <span>{value || '根目錄（留空）'}</span>
        <svg
          width="12" height="12" viewBox="0 0 24 24" fill="none"
          stroke="rgba(255,255,255,0.4)" strokeWidth="2.5"
          strokeLinecap="round" strokeLinejoin="round"
          style={{ flexShrink: 0, transform: open ? 'rotate(180deg)' : 'none', transition: 'transform 0.2s' }}
        >
          <polyline points="6 9 12 15 18 9"/>
        </svg>
      </button>

      {/* inline 展開清單 */}
      {open && (
        <div style={{
          marginTop: 4,
          borderRadius: 8,
          border: '1px solid rgba(255,255,255,0.12)',
          background: 'rgba(20,20,28,0.98)',
          overflow: 'hidden',
          maxHeight: 200,
          overflowY: 'auto',
        }}>
          {options.map(f => {
            const depth    = f ? f.split('/').length : 0
            const label    = f ? f.split('/').pop()! : '根目錄'
            const isActive = f === value
            return (
              <button
                key={f || '__root__'}
                onClick={() => { onChange(f); setOpen(false) }}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  width: '100%',
                  padding: '10px 12px',
                  paddingLeft: 12 + depth * 16,
                  borderBottom: '1px solid rgba(255,255,255,0.05)',
                  background: isActive ? 'rgba(99,102,241,0.2)' : 'transparent',
                  border: 'none',
                  color: isActive ? 'var(--color-accent, #6366f1)' : 'rgba(255,255,255,0.75)',
                  fontSize: '13px',
                  textAlign: 'left',
                  cursor: 'pointer',
                  gap: 6,
                  lineHeight: 1.4,
                }}
              >
                {depth > 0 && (
                  <span style={{ opacity: 0.35, fontSize: '11px', fontFamily: 'monospace', flexShrink: 0 }}>
                    {'└─'}
                  </span>
                )}
                <span style={{ flex: 1 }}>{label}</span>
                {isActive && (
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" style={{ flexShrink: 0 }}>
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                )}
              </button>
            )
          })}
        </div>
      )}
    </div>
  )
}

interface VoiceOverlayProps {
  voiceState: VoiceState
  transcript: string
  preview?: string | null       // 臨時預覽（每 5 秒更新，VAD flush 後清除）
  previewEnabled?: boolean      // false → 不送 whisper 預覽，改顯示「語音辨識中…」動畫
  isSpeaking?: boolean          // VAD 偵測到有聲音（說話中）
  onConfirm: () => void
  onDiscard: () => void
  onSaveToCurrentNote?: () => void              // undefined → 無開啟筆記，按鈕 disabled
  onSaveToNewNote?: (title: string, folder: string) => void
}

/** preview 關閉時的佔位動畫：三點 CSS 波浪，無 JS timer */
function RecordingStatus({ voiceState }: { voiceState: VoiceState }) {
  const label = voiceState === 'transcribing' ? '辨識中' : '語音辨識中'
  const dot = (delay: string) => (
    <span style={{ animation: `voice-dot-blink 1.4s ease-in-out ${delay} infinite` }}>.</span>
  )
  return (
    <span style={{ color: 'rgba(255,255,255,0.3)' }}>
      {label}{dot('0ms')}{dot('230ms')}{dot('460ms')}
    </span>
  )
}

function makeDefaultTitle(): string {
  const now = new Date()
  const pad = (n: number) => String(n).padStart(2, '0')
  return `語音記錄 ${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())} ${pad(now.getHours())}${pad(now.getMinutes())}`
}

export default function VoiceOverlay({
  voiceState,
  transcript,
  preview,
  previewEnabled = true,
  isSpeaking = false,
  onConfirm,
  onDiscard,
  onSaveToCurrentNote,
  onSaveToNewNote,
}: VoiceOverlayProps) {
  const isRecording    = voiceState === 'recording'
  const isTranscribing = voiceState === 'transcribing'
  const textRef       = useRef<HTMLDivElement>(null)
  const titleInputRef = useRef<HTMLInputElement>(null)

  // ── view 切換狀態 ────────────────────────────────────────────────────────────
  const [view, setView]               = useState<'main' | 'new_note'>('main')
  const [newNoteTitle, setNewNoteTitle] = useState('')
  const [newNoteFolder, setNewNoteFolder] = useState('')

  // 轉錄文字增長時自動捲動到底部
  useEffect(() => {
    const el = textRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [transcript])

  // preview 模式下，捲動跟隨 preview 文字
  useEffect(() => {
    if (!previewEnabled) return
    const el = textRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [preview, previewEnabled])

  // view 切換到 new_note 後，等動畫完成再 focus（避免 scrollIntoView 把頁面捲走）
  useEffect(() => {
    if (view !== 'new_note') return
    const timer = setTimeout(() => titleInputRef.current?.focus(), 320)
    return () => clearTimeout(timer)
  }, [view])

  // ── 開啟新筆記表單 ───────────────────────────────────────────────────────────
  const handleOpenNewNote = () => {
    setNewNoteTitle(makeDefaultTitle())
    setNewNoteFolder('')
    setView('new_note')
  }

  const handleConfirmNewNote = () => {
    onSaveToNewNote?.(newNoteTitle.trim(), newNoteFolder.trim())
  }

  // ── 共用樣式 ────────────────────────────────────────────────────────────────
  const inputStyle: React.CSSProperties = {
    width: '100%',
    padding: '9px 12px',
    borderRadius: 8,
    border: '1px solid rgba(255,255,255,0.15)',
    background: 'rgba(255,255,255,0.07)',
    color: 'rgba(255,255,255,0.9)',
    fontSize: '13px',
    outline: 'none',
    boxSizing: 'border-box',
  }
  const labelStyle: React.CSSProperties = {
    display: 'block',
    fontSize: '11px',
    fontWeight: 600,
    letterSpacing: '0.06em',
    textTransform: 'uppercase',
    color: 'rgba(255,255,255,0.4)',
    marginBottom: 6,
  }

  const SLIDE = 'transform 300ms cubic-bezier(0.4, 0, 0.2, 1)'

  return (
    <div style={{
      position: 'absolute',
      inset: 0,
      zIndex: 100,
      overflow: 'hidden',
      background: 'rgba(8, 8, 12, 0.93)',
      backdropFilter: 'blur(14px)',
      WebkitBackdropFilter: 'blur(14px)',
      animation: 'voice-overlay-in 220ms cubic-bezier(0, 0, 0.2, 1) both',
    }}>

      {/* ══ Main view ══════════════════════════════════════════════════════════ */}
      <div style={{
        position: 'absolute',
        inset: 0,
        display: 'flex',
        flexDirection: 'column',
        transform: view === 'new_note' ? 'translateX(-100%)' : 'translateX(0)',
        transition: SLIDE,
      }}>

        {/* ── 狀態列 ── */}
        <div style={{
          padding: '16px 18px 12px',
          display: 'flex',
          alignItems: 'center',
          gap: 10,
          borderBottom: '1px solid rgba(255,255,255,0.07)',
          flexShrink: 0,
        }}>
          {isRecording ? (
            <span style={{ position: 'relative', display: 'inline-flex', width: 18, height: 18, alignItems: 'center', justifyContent: 'center', flexShrink: 0 }}>
              <span style={{ position: 'absolute', width: 18, height: 18, borderRadius: '50%', background: '#ef4444', opacity: 0.25, animation: 'voice-dot-ring 1.4s ease-in-out infinite' }} />
              <span style={{ width: 10, height: 10, borderRadius: '50%', background: '#ef4444', flexShrink: 0 }} />
            </span>
          ) : isTranscribing ? (
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="rgba(255,255,255,0.4)" strokeWidth="2.5" strokeLinecap="round" style={{ flexShrink: 0 }}>
              <circle cx="12" cy="12" r="9" strokeDasharray="28 56">
                <animateTransform attributeName="transform" type="rotate" from="0 12 12" to="360 12 12" dur="0.8s" repeatCount="indefinite"/>
              </circle>
            </svg>
          ) : (
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#30d158" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" style={{ flexShrink: 0 }}>
              <polyline points="20 6 9 17 4 12"/>
            </svg>
          )}
          <span style={{
            fontSize: '12px',
            fontWeight: 600,
            letterSpacing: '0.06em',
            textTransform: 'uppercase',
            color: isRecording ? '#ef4444' : isTranscribing ? 'rgba(255,255,255,0.4)' : '#30d158',
          }}>
            {isRecording ? '錄音中' : isTranscribing ? '辨識中…' : '辨識完成'}
          </span>
        </div>

        {/* ── 轉錄文字 ── */}
        <div
          ref={textRef}
          style={{
            flex: 1,
            padding: '16px 18px',
            overflowY: 'auto',
            fontFamily: 'var(--font-sans)',
            fontSize: '15px',
            lineHeight: 1.8,
            color: transcript ? 'rgba(255,255,255,0.90)' : 'rgba(255,255,255,0.2)',
            whiteSpace: 'pre-wrap',
            wordBreak: 'break-word',
            letterSpacing: '0.01em',
          }}
        >
          {transcript}
          {previewEnabled ? (
            !transcript && !preview
              ? '說話後文字將顯示在這裡…'
              : preview && (
                <span style={{ color: 'rgba(255,255,255,0.35)' }}>
                  {transcript ? ' ' : ''}{preview}
                </span>
              )
          ) : (
            (isSpeaking || isTranscribing) && (
              <RecordingStatus voiceState={voiceState} />
            )
          )}
        </div>

        {/* ── 操作按鈕（2×2 網格）── */}
        <div style={{
          padding: '10px 16px 20px',
          display: 'grid',
          gridTemplateColumns: '1fr 1fr',
          gap: 8,
          flexShrink: 0,
          borderTop: '1px solid rgba(255,255,255,0.07)',
        }}>
          {/* Row 1：捨棄 | 填入輸入框 */}
          <button
            onClick={onDiscard}
            style={{
              padding: '11px 0',
              borderRadius: 10,
              border: '1px solid rgba(255,255,255,0.12)',
              background: 'rgba(255,255,255,0.05)',
              color: 'rgba(255,255,255,0.65)',
              fontSize: '13px',
              fontWeight: 500,
              cursor: 'pointer',
            }}
          >
            捨棄
          </button>
          <button
            onClick={onConfirm}
            style={{
              padding: '11px 0',
              borderRadius: 10,
              border: 'none',
              background: 'var(--color-accent, #6366f1)',
              color: 'white',
              fontSize: '13px',
              fontWeight: 600,
              cursor: 'pointer',
              opacity: isTranscribing ? 0.8 : 1,
              transition: 'opacity 0.15s',
            }}
          >
            {isRecording ? '停止並填入' : isTranscribing ? '完成後填入…' : '填入輸入框'}
          </button>

          {/* Row 2：寫入當前筆記 | 寫入新筆記 */}
          <button
            onClick={onSaveToCurrentNote}
            disabled={!onSaveToCurrentNote}
            style={{
              padding: '10px 0',
              borderRadius: 10,
              border: '1px solid rgba(255,255,255,0.12)',
              background: 'rgba(255,255,255,0.07)',
              color: onSaveToCurrentNote ? 'rgba(255,255,255,0.8)' : 'rgba(255,255,255,0.25)',
              fontSize: '12px',
              fontWeight: 500,
              cursor: onSaveToCurrentNote ? 'pointer' : 'not-allowed',
            }}
          >
            寫入當前筆記
          </button>
          <button
            onClick={handleOpenNewNote}
            style={{
              padding: '10px 0',
              borderRadius: 10,
              border: '1px solid rgba(255,255,255,0.12)',
              background: 'rgba(255,255,255,0.07)',
              color: 'rgba(255,255,255,0.8)',
              fontSize: '12px',
              fontWeight: 500,
              cursor: 'pointer',
            }}
          >
            寫入新筆記
          </button>
        </div>
      </div>

      {/* ══ New note form view ════════════════════════════════════════════════ */}
      <div style={{
        position: 'absolute',
        inset: 0,
        display: 'flex',
        flexDirection: 'column',
        transform: view === 'new_note' ? 'translateX(0)' : 'translateX(100%)',
        transition: SLIDE,
      }}>

        {/* ── 頂欄：返回 + 頁面標題 ── */}
        <div style={{
          padding: '12px 16px',
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          borderBottom: '1px solid rgba(255,255,255,0.07)',
          flexShrink: 0,
        }}>
          <button
            onClick={() => setView('main')}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 3,
              background: 'none',
              border: 'none',
              color: 'var(--color-accent, #6366f1)',
              fontSize: '13px',
              fontWeight: 500,
              cursor: 'pointer',
              padding: '4px 0',
              flexShrink: 0,
            }}
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <polyline points="15 18 9 12 15 6"/>
            </svg>
            返回
          </button>
          <span style={{
            flex: 1,
            textAlign: 'center',
            fontSize: '13px',
            fontWeight: 600,
            color: 'rgba(255,255,255,0.75)',
            marginRight: 40,
          }}>
            新筆記
          </span>
        </div>

        {/* ── 表單欄位 ── */}
        <div style={{
          flex: 1,
          padding: '20px 16px',
          display: 'flex',
          flexDirection: 'column',
          gap: 18,
          overflowY: 'auto',
        }}>
          <div>
            <label style={labelStyle}>寫入位置</label>
            <FolderPicker
              value={newNoteFolder}
              onChange={setNewNoteFolder}
              inputStyle={inputStyle}
            />
          </div>
          <div>
            <label style={labelStyle}>筆記名稱</label>
            {/* 不用 autoFocus：off-screen 的 autoFocus 會觸發 scrollIntoView，把整個頁面捲走 */}
            <input
              ref={titleInputRef}
              type="text"
              value={newNoteTitle}
              onChange={(e) => setNewNoteTitle(e.target.value)}
              placeholder="請輸入筆記名稱"
              style={inputStyle}
            />
          </div>
        </div>

        {/* ── 確認按鈕 ── */}
        <div style={{
          padding: '10px 16px 20px',
          flexShrink: 0,
          borderTop: '1px solid rgba(255,255,255,0.07)',
        }}>
          <button
            onClick={handleConfirmNewNote}
            disabled={!newNoteTitle.trim()}
            style={{
              width: '100%',
              padding: '12px 0',
              borderRadius: 10,
              border: 'none',
              background: newNoteTitle.trim() ? 'var(--color-accent, #6366f1)' : 'rgba(255,255,255,0.08)',
              color: newNoteTitle.trim() ? 'white' : 'rgba(255,255,255,0.25)',
              fontSize: '13px',
              fontWeight: 600,
              cursor: newNoteTitle.trim() ? 'pointer' : 'not-allowed',
              opacity: isTranscribing ? 0.8 : 1,
              transition: 'opacity 0.15s, background 0.15s',
            }}
          >
            {isRecording ? '停止並建立' : isTranscribing ? '完成後建立…' : '確認建立'}
          </button>
        </div>
      </div>
    </div>
  )
}
