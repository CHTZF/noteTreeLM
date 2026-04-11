import { useState } from 'react'
import { Message } from './types'

export default function MessageBubble({
  message,
  streaming,
  onSaveWeb,
  savingWeb,
  onExtractSkill,
  rating,
  onRate,
}: {
  message: Message
  streaming?: boolean
  onSaveWeb?: () => void
  savingWeb?: boolean
  onExtractSkill?: () => void
  rating?: 'good' | 'bad'
  onRate?: (r: 'good' | 'bad') => void
}) {
  const isUser = message.role === 'user'
  const isTool = message.role === 'tool'
  const isNotice = message.role === 'notice'
  const isThink = message.role === 'think'
  const citeFailed = message.citeFailed
  const citeDetails = message.citeDetails
  // citeFailed === undefined → no cite_status received (pure text, no tools ran) → hide badge
  // citeFailed === false    → cite_status passed → show ✓
  // citeFailed === true     → cite_status failed → show ⚠
  const hasCiteInfo = citeFailed !== undefined
  const [citeExpanded, setCiteExpanded] = useState(false)

  if (isThink) {
    return (
      <div style={{ display: 'flex', justifyContent: 'flex-start', marginBottom: 2 }}>
        <details style={{ maxWidth: '92%' }}>
          <summary style={{
            fontSize: '11px', color: 'var(--color-text-muted)',
            cursor: 'pointer', userSelect: 'none', listStyle: 'none',
            display: 'flex', alignItems: 'center', gap: 4,
            opacity: 0.6,
          }}>
            <span style={{ fontSize: 9 }}>▶</span>
            <span style={{ fontStyle: 'italic' }}>💭 {message.content.slice(0, 40)}{message.content.length > 40 ? '…' : ''}</span>
          </summary>
          <div style={{
            marginTop: 4, padding: '4px 10px',
            borderLeft: '2px solid var(--color-border)',
            color: 'var(--color-text-muted)',
            fontSize: '11px', fontStyle: 'italic',
            whiteSpace: 'pre-wrap',
          }}>
            {message.content}
          </div>
        </details>
      </div>
    )
  }

  if (isNotice) {
    return (
      <div style={{
        textAlign: 'center', fontSize: '11px', color: 'var(--color-text-muted)',
        padding: '4px 0', opacity: 0.7,
      }}>
        ── {message.content} ──
      </div>
    )
  }

  if (isTool) {
    return (
      <div style={{ display: 'flex', justifyContent: 'flex-start' }}>
        <div style={{
          maxWidth: '92%', padding: '4px 10px', borderRadius: '6px',
          background: 'var(--color-bg-overlay)',
          color: 'var(--color-text-muted)',
          fontSize: '11px', lineHeight: 1.5,
          fontFamily: 'var(--font-mono, monospace)',
          border: '1px solid var(--color-border)',
          whiteSpace: 'pre-wrap',
        }}>
          {message.content}
        </div>
      </div>
    )
  }

  return (
    <div style={{ display: 'flex', justifyContent: isUser ? 'flex-end' : 'flex-start' }}>
      <div style={{ maxWidth: '85%', display: 'flex', flexDirection: 'column', alignItems: isUser ? 'flex-end' : 'flex-start' }}>
        <div style={{
          padding: '8px 12px',
          borderRadius: isUser ? '12px 12px 4px 12px' : '12px 12px 12px 4px',
          background: isUser ? 'var(--color-accent)' : 'var(--color-bg-elevated)',
          color: isUser ? 'white' : 'var(--color-text-primary)',
          fontSize: '13px', lineHeight: 1.6, wordBreak: 'break-word',
          border: isUser ? 'none' : '1px solid var(--color-border)',
          whiteSpace: 'pre-wrap',
        }}>
          {message.content}
          {streaming && (
            <span style={{
              display: 'inline-block', width: '2px', height: '14px',
              background: 'var(--color-accent)', marginLeft: '2px', verticalAlign: 'text-bottom',
              animation: 'blink 1s step-start infinite',
            }} />
          )}
          {streaming && (
            <style>{`@keyframes blink { 0%, 100% { opacity: 1 } 50% { opacity: 0 } }`}</style>
          )}
        </div>
        {!isUser && !isTool && !isNotice && hasCiteInfo && (
          <div style={{ marginTop: '4px' }}>
            <button
              onClick={() => setCiteExpanded(v => !v)}
              title={citeFailed ? '此回覆未通過引用驗證，內容可能包含未經確認的資料' : '點選展開引用來源'}
              style={{
                display: 'inline-flex', alignItems: 'center', gap: '4px',
                fontSize: '11px', padding: '2px 7px', borderRadius: '10px',
                background: citeFailed ? 'rgba(255,159,10,0.12)' : 'rgba(48,209,88,0.10)',
                border: `1px solid ${citeFailed ? 'rgba(255,159,10,0.35)' : 'rgba(48,209,88,0.35)'}`,
                color: citeFailed ? '#ff9f0a' : '#30d158',
                cursor: 'pointer', userSelect: 'none',
              }}
            >
              {citeFailed ? '⚠ 引用未驗證' : '✓ 引用已驗證'}
              {!citeFailed && citeDetails && citeDetails.length > 0 && (
                <span style={{ opacity: 0.7 }}>{citeExpanded ? '▲' : '▼'}</span>
              )}
            </button>
            {citeExpanded && !citeFailed && citeDetails && citeDetails.length > 0 && (
              <div style={{
                marginTop: '6px',
                display: 'flex', flexDirection: 'column', gap: '6px',
              }}>
                {citeDetails.map(d => (
                  <div key={d.id} style={{
                    padding: '6px 10px', borderRadius: '6px',
                    background: 'var(--color-bg-overlay)',
                    border: '1px solid var(--color-border)',
                    fontSize: '11px', lineHeight: 1.5,
                  }}>
                    <div style={{
                      fontFamily: 'var(--font-mono, monospace)',
                      color: 'var(--color-text-muted)',
                      marginBottom: '2px',
                    }}>
                      📎 {d.tool}
                    </div>
                    <div style={{
                      color: 'var(--color-text-secondary)',
                      whiteSpace: 'pre-wrap', wordBreak: 'break-all',
                      maxHeight: '120px', overflowY: 'auto',
                    }}>
                      {d.preview}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
        {!isUser && !isTool && !isNotice && (onSaveWeb || onExtractSkill || onRate) && (
          <div style={{ display: 'flex', gap: '4px', marginTop: '2px', alignItems: 'center' }}>
            {onSaveWeb && (
              <button
                className={message.savedWeb ? 'import-panel-v2__saved-btn' : 'import-panel-v2__save-btn'}
                onClick={message.savedWeb ? undefined : onSaveWeb}
                disabled={savingWeb || message.savedWeb}
              >
                {message.savedWeb ? '✓ 已儲存' : savingWeb ? '儲存中…' : '儲存為知識'}
              </button>
            )}
            {onExtractSkill && (
              <button
                onClick={onExtractSkill}
                title="從此回覆萃取技能規範"
                style={{
                  fontSize: '11px', padding: '2px 8px', borderRadius: '10px',
                  background: 'transparent', border: '1px solid var(--color-border)',
                  color: 'var(--color-text-muted)', cursor: 'pointer',
                }}
              >📌 轉為技能規範</button>
            )}
            {onRate && (
              <>
                <button
                  onClick={() => !rating && onRate('good')}
                  title="這個回覆有幫助"
                  style={{
                    fontSize: '11px', padding: '2px 6px', borderRadius: '10px',
                    background: rating === 'good' ? 'rgba(48,209,88,0.15)' : 'transparent',
                    border: `1px solid ${rating === 'good' ? 'rgba(48,209,88,0.5)' : 'var(--color-border)'}`,
                    color: rating === 'good' ? '#30d158' : 'var(--color-text-muted)',
                    cursor: rating ? 'default' : 'pointer',
                  }}
                >👍</button>
                <button
                  onClick={() => !rating && onRate('bad')}
                  title="這個回覆不夠好"
                  style={{
                    fontSize: '11px', padding: '2px 6px', borderRadius: '10px',
                    background: rating === 'bad' ? 'rgba(255,69,58,0.15)' : 'transparent',
                    border: `1px solid ${rating === 'bad' ? 'rgba(255,69,58,0.5)' : 'var(--color-border)'}`,
                    color: rating === 'bad' ? '#ff453a' : 'var(--color-text-muted)',
                    cursor: rating ? 'default' : 'pointer',
                  }}
                >👎</button>
              </>
            )}
          </div>
        )}
      </div>
    </div>
  )
}
