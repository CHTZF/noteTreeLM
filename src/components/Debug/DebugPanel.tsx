import { useRef, useEffect, useState } from 'react'
import { useDebugStore, type DebugLevel } from '../../stores/debugStore'

const LEVEL_COLOR: Record<DebugLevel, string> = {
  info:  'var(--color-text-secondary)',
  warn:  'var(--color-warning, #f59e0b)',
  error: 'var(--color-error, #ef4444)',
}

const LEVEL_BG: Record<DebugLevel, string> = {
  info:  'transparent',
  warn:  'rgba(245,158,11,0.06)',
  error: 'rgba(239,68,68,0.08)',
}

const LEVEL_LABEL: Record<DebugLevel, string> = {
  info:  'INFO',
  warn:  'WARN',
  error: 'ERR ',
}

function LogMessage({ message, color }: { message: string; color: string }) {
  const [expanded, setExpanded] = useState(false)
  const isMultiline = message.includes('\n')
  const firstLine = isMultiline ? message.slice(0, message.indexOf('\n')) : message
  const rest = isMultiline ? message.slice(message.indexOf('\n') + 1) : ''

  return (
    <span style={{ fontFamily: 'var(--font-mono)', fontSize: '11px', color, lineHeight: 1.6, wordBreak: 'break-all' }}>
      {firstLine}
      {isMultiline && (
        <>
          {' '}
          <button
            onClick={() => setExpanded((v) => !v)}
            style={{
              fontSize: '10px', padding: '0 5px', borderRadius: '3px', cursor: 'pointer',
              background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
              color: 'var(--color-text-muted)', lineHeight: '16px', verticalAlign: 'middle',
            }}
          >
            {expanded ? '▲ 收起' : '▼ 展開'}
          </button>
          {expanded && (
            <pre style={{
              margin: '4px 0 2px', padding: '6px 8px', borderRadius: '4px',
              background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
              fontSize: '10px', lineHeight: 1.5, whiteSpace: 'pre-wrap', wordBreak: 'break-all',
              color,
            }}>
              {rest}
            </pre>
          )}
        </>
      )}
    </span>
  )
}

export default function DebugPanel() {
  const { entries, clear } = useDebugStore()
  const bottomRef = useRef<HTMLDivElement>(null)

  // Auto-scroll to bottom on new entries
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [entries.length])

  return (
    <div style={{
      display: 'flex', flexDirection: 'column', height: '100%',
      background: 'var(--color-bg-base)',
    }}>
      {/* Header */}
      <div style={{
        display: 'flex', alignItems: 'center', justifyContent: 'space-between',
        padding: '8px 12px', borderBottom: '1px solid var(--color-border)',
        flexShrink: 0,
      }}>
        <span style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)', letterSpacing: '0.05em' }}>
          DEBUG LOG
        </span>
        <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
          <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>
            {entries.length} 筆
          </span>
          <button
            onClick={clear}
            style={{
              fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
              background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
              color: 'var(--color-text-secondary)', cursor: 'pointer',
            }}
          >清除</button>
        </div>
      </div>

      {/* Log entries */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '4px 0' }}>
        {entries.length === 0 ? (
          <div style={{
            display: 'flex', alignItems: 'center', justifyContent: 'center',
            height: '100%', color: 'var(--color-text-muted)', fontSize: '12px',
          }}>
            尚無日誌。點擊工具列的 🎙 按鈕開始錄音。
          </div>
        ) : (
          entries.map((entry) => (
            <div
              key={entry.id}
              style={{
                display: 'flex', gap: '6px', alignItems: 'flex-start',
                padding: '3px 10px',
                background: LEVEL_BG[entry.level],
                borderLeft: entry.level !== 'info'
                  ? `2px solid ${LEVEL_COLOR[entry.level]}`
                  : '2px solid transparent',
              }}
            >
              {/* Timestamp */}
              <span style={{
                flexShrink: 0, fontFamily: 'var(--font-mono)', fontSize: '10px',
                color: 'var(--color-text-muted)', paddingTop: '1px', lineHeight: 1.6,
                userSelect: 'none',
              }}>
                {entry.timestamp}
              </span>

              {/* Level badge */}
              <span style={{
                flexShrink: 0, fontFamily: 'var(--font-mono)', fontSize: '10px',
                color: LEVEL_COLOR[entry.level],
                paddingTop: '1px', lineHeight: 1.6,
                userSelect: 'none', fontWeight: 600, letterSpacing: '0.04em',
              }}>
                {LEVEL_LABEL[entry.level]}
              </span>

              {/* Category */}
              <span style={{
                flexShrink: 0, fontSize: '10px',
                color: 'var(--color-accent)',
                paddingTop: '1px', lineHeight: 1.6,
                userSelect: 'none',
              }}>
                [{entry.category}]
              </span>

              {/* Message */}
              <LogMessage message={entry.message} color={LEVEL_COLOR[entry.level]} />
            </div>
          ))
        )}
        <div ref={bottomRef} />
      </div>
    </div>
  )
}
