import { useRef, useEffect, useState, useMemo } from 'react'
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

const ALL_LEVELS: DebugLevel[] = ['info', 'warn', 'error']

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
  const scrollRef = useRef<HTMLDivElement>(null)
  const atBottomRef = useRef(true)

  const [search, setSearch] = useState('')
  const [activeCategory, setActiveCategory] = useState<string | null>(null)
  const [hiddenLevels, setHiddenLevels] = useState<Set<DebugLevel>>(new Set())

  // Unique categories derived from all entries
  const categories = useMemo(() => {
    const seen = new Set<string>()
    for (const e of entries) seen.add(e.category)
    return Array.from(seen).sort()
  }, [entries])

  // Filtered entries
  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase()
    return entries.filter((e) => {
      if (activeCategory && e.category !== activeCategory) return false
      if (hiddenLevels.has(e.level)) return false
      if (q && !e.message.toLowerCase().includes(q) && !e.category.toLowerCase().includes(q)) return false
      return true
    })
  }, [entries, activeCategory, hiddenLevels, search])

  // Track scroll position to decide if we should auto-scroll
  const handleScroll = () => {
    const el = scrollRef.current
    if (!el) return
    atBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40
  }

  // Auto-scroll to bottom only when no filter is active and we're near bottom
  useEffect(() => {
    const isFiltering = search.trim() || activeCategory || hiddenLevels.size > 0
    if (!isFiltering && atBottomRef.current) {
      bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
    }
  }, [entries.length])

  const toggleLevel = (level: DebugLevel) => {
    setHiddenLevels((prev) => {
      const next = new Set(prev)
      if (next.has(level)) next.delete(level)
      else next.add(level)
      return next
    })
  }

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
            {filtered.length}/{entries.length} 筆
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

      {/* Search */}
      <div style={{ padding: '6px 10px', borderBottom: '1px solid var(--color-border)', flexShrink: 0 }}>
        <input
          type="text"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder="搜尋訊息或分類…"
          style={{
            width: '100%', boxSizing: 'border-box',
            padding: '4px 8px', fontSize: '11px',
            background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
            borderRadius: '5px', color: 'var(--color-text-primary)', outline: 'none',
          }}
        />
      </div>

      {/* Category chips + Level toggles */}
      <div style={{ padding: '5px 10px', borderBottom: '1px solid var(--color-border)', flexShrink: 0, display: 'flex', flexDirection: 'column', gap: 4 }}>
        {/* Category chips */}
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4 }}>
          <button
            onClick={() => setActiveCategory(null)}
            style={{
              fontSize: '10px', padding: '1px 7px', borderRadius: '10px', cursor: 'pointer',
              border: '1px solid var(--color-border)',
              background: activeCategory === null ? 'var(--color-accent)' : 'var(--color-bg-overlay)',
              color: activeCategory === null ? '#fff' : 'var(--color-text-secondary)',
              fontWeight: activeCategory === null ? 600 : 400,
            }}
          >全部</button>
          {categories.map((cat) => (
            <button
              key={cat}
              onClick={() => setActiveCategory(activeCategory === cat ? null : cat)}
              style={{
                fontSize: '10px', padding: '1px 7px', borderRadius: '10px', cursor: 'pointer',
                border: '1px solid var(--color-border)',
                background: activeCategory === cat ? 'var(--color-accent)' : 'var(--color-bg-overlay)',
                color: activeCategory === cat ? '#fff' : 'var(--color-text-muted)',
                fontWeight: activeCategory === cat ? 600 : 400,
              }}
            >{cat}</button>
          ))}
        </div>

        {/* Level toggles */}
        <div style={{ display: 'flex', gap: 4 }}>
          {ALL_LEVELS.map((level) => {
            const hidden = hiddenLevels.has(level)
            return (
              <button
                key={level}
                onClick={() => toggleLevel(level)}
                style={{
                  fontSize: '10px', padding: '1px 7px', borderRadius: '4px', cursor: 'pointer',
                  border: `1px solid ${hidden ? 'var(--color-border)' : LEVEL_COLOR[level]}`,
                  background: hidden ? 'var(--color-bg-overlay)' : `${LEVEL_COLOR[level]}22`,
                  color: hidden ? 'var(--color-text-muted)' : LEVEL_COLOR[level],
                  fontFamily: 'var(--font-mono)', fontWeight: 600,
                  opacity: hidden ? 0.5 : 1,
                }}
              >{LEVEL_LABEL[level].trim()}</button>
            )
          })}
        </div>
      </div>

      {/* Log entries */}
      <div ref={scrollRef} onScroll={handleScroll} style={{ flex: 1, overflowY: 'auto', padding: '4px 0' }}>
        {filtered.length === 0 ? (
          <div style={{
            display: 'flex', alignItems: 'center', justifyContent: 'center',
            height: '100%', color: 'var(--color-text-muted)', fontSize: '12px',
          }}>
            {entries.length === 0 ? '尚無日誌。' : '無符合的日誌。'}
          </div>
        ) : (
          filtered.map((entry) => (
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
