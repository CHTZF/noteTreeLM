import { useEffect, useRef, useState } from 'react'
import type { VoiceState } from '../../hooks/useVoiceRecorder'

interface VoiceOverlayProps {
  voiceState: VoiceState
  transcript: string
  preview?: string | null     // 臨時預覽（每 5 秒更新，VAD flush 後清除）
  previewEnabled?: boolean    // false → 不送 whisper 預覽，改顯示「語音辨識中…」動畫
  isSpeaking?: boolean        // VAD 偵測到有聲音（說話中）
  onConfirm: () => void
  onDiscard: () => void
}

/** preview 關閉時的佔位動畫：點點循環 + 每 5 秒 fade-in 重置 */
function RecordingStatus({ voiceState }: { voiceState: VoiceState }) {
  const [dots, setDots] = useState('...')
  const [animKey, setAnimKey] = useState(0)

  useEffect(() => {
    if (voiceState !== 'recording') return
    const dotsTimer = setInterval(() => {
      setDots((d) => (d.length >= 3 ? '' : d + '.'))
    }, 400)
    const refreshTimer = setInterval(() => {
      setAnimKey((k) => k + 1)
    }, 5000)
    return () => {
      clearInterval(dotsTimer)
      clearInterval(refreshTimer)
    }
  }, [voiceState])

  if (voiceState === 'transcribing') {
    return <span style={{ color: 'rgba(255,255,255,0.3)', fontStyle: 'italic' }}>辨識中…</span>
  }

  return (
    <span
      key={animKey}
      style={{
        color: 'rgba(255,255,255,0.3)',
        fontStyle: 'italic',
        animation: 'voice-overlay-in 400ms ease-out both',
      }}
    >
      語音辨識中{dots}
    </span>
  )
}

export default function VoiceOverlay({
  voiceState,
  transcript,
  preview,
  previewEnabled = true,
  isSpeaking = false,
  onConfirm,
  onDiscard,
}: VoiceOverlayProps) {
  const isRecording    = voiceState === 'recording'
  const isTranscribing = voiceState === 'transcribing'
  const textRef = useRef<HTMLDivElement>(null)

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

  return (
    <div style={{
      position: 'absolute',
      inset: 0,
      zIndex: 100,
      display: 'flex',
      flexDirection: 'column',
      background: 'rgba(8, 8, 12, 0.93)',
      backdropFilter: 'blur(14px)',
      WebkitBackdropFilter: 'blur(14px)',
      animation: 'voice-overlay-in 220ms cubic-bezier(0, 0, 0.2, 1) both',
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
          /* 紅色脈衝圓點 */
          <span style={{ position: 'relative', display: 'inline-flex', width: 18, height: 18, alignItems: 'center', justifyContent: 'center', flexShrink: 0 }}>
            <span style={{ position: 'absolute', width: 18, height: 18, borderRadius: '50%', background: '#ef4444', opacity: 0.25, animation: 'voice-dot-ring 1.4s ease-in-out infinite' }} />
            <span style={{ width: 10, height: 10, borderRadius: '50%', background: '#ef4444', flexShrink: 0 }} />
          </span>
        ) : isTranscribing ? (
          /* 旋轉圈 */
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="rgba(255,255,255,0.4)" strokeWidth="2.5" strokeLinecap="round" style={{ flexShrink: 0 }}>
            <circle cx="12" cy="12" r="9" strokeDasharray="28 56">
              <animateTransform attributeName="transform" type="rotate" from="0 12 12" to="360 12 12" dur="0.8s" repeatCount="indefinite"/>
            </circle>
          </svg>
        ) : (
          /* 綠色打勾 */
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
          fontSize: '15px',
          lineHeight: 1.8,
          color: transcript ? 'rgba(255,255,255,0.90)' : 'rgba(255,255,255,0.2)',
          whiteSpace: 'pre-wrap',
          wordBreak: 'break-word',
          letterSpacing: '0.01em',
        }}
      >
        {/* 已確認的轉錄文字 */}
        {transcript}

        {previewEnabled ? (
          /* Preview 開啟：顯示臨時辨識結果（淡色斜體）或佔位提示 */
          !transcript && !preview
            ? '說話後文字將顯示在這裡…'
            : preview && (
              <span style={{ color: 'rgba(255,255,255,0.35)', fontStyle: 'italic' }}>
                {transcript ? ' ' : ''}{preview}
              </span>
            )
        ) : (
          /* Preview 關閉：說話中或 VAD 觸發辨識中才顯示；靜音等待時不顯示 */
          (isSpeaking || isTranscribing) && (
            <RecordingStatus voiceState={voiceState} />
          )
        )}
      </div>

      {/* ── 操作按鈕 ── */}
      <div style={{
        padding: '10px 16px 20px',
        display: 'flex',
        gap: 10,
        flexShrink: 0,
        borderTop: '1px solid rgba(255,255,255,0.07)',
      }}>
        <button
          onClick={onDiscard}
          style={{
            flex: 1,
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
            flex: 2,
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
      </div>
    </div>
  )
}
