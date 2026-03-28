import { useState, useEffect, useRef, useMemo } from 'react'
import { useVaultStore } from '../../stores/vaultStore'

export interface SpotlightFeature {
  id: string
  label: string
  icon: string
  action: () => void
}

interface SpotlightProps {
  features: SpotlightFeature[]
  onSelectNote: (path: string) => void
  onClose: () => void
}

type ResultItem =
  | { kind: 'feature'; feature: SpotlightFeature }
  | { kind: 'note'; path: string; title: string }

export default function Spotlight({ features, onSelectNote, onClose }: SpotlightProps) {
  const [query, setQuery] = useState('')
  const [selectedIndex, setSelectedIndex] = useState(0)
  const { notes } = useVaultStore()
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => { inputRef.current?.focus() }, [])

  const results = useMemo<ResultItem[]>(() => {
    const q = query.toLowerCase().trim()
    const matchedFeatures: ResultItem[] = features
      .filter(f => !q || f.label.toLowerCase().includes(q))
      .map(f => ({ kind: 'feature', feature: f }))
    const matchedNotes: ResultItem[] = notes
      .filter(n => !q || n.title.toLowerCase().includes(q) || n.path.toLowerCase().includes(q))
      .slice(0, 8)
      .map(n => ({ kind: 'note', path: n.path, title: n.title }))
    return [...matchedFeatures, ...matchedNotes].slice(0, 12)
  }, [query, features, notes])

  useEffect(() => { setSelectedIndex(0) }, [query])

  const handleSelect = (item: ResultItem) => {
    if (item.kind === 'feature') { onClose(); item.feature.action() }
    else { onSelectNote(item.path); onClose() }
  }

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); setSelectedIndex(i => Math.min(i + 1, results.length - 1)) }
    if (e.key === 'ArrowUp') { e.preventDefault(); setSelectedIndex(i => Math.max(i - 1, 0)) }
    if (e.key === 'Enter' && results[selectedIndex]) handleSelect(results[selectedIndex])
    if (e.key === 'Escape') onClose()
  }

  // Group: features first, then notes
  const firstNoteIdx = results.findIndex(r => r.kind === 'note')

  return (
    <div
      style={{
        position: 'fixed', inset: 0, zIndex: 2000,
        background: 'rgba(0,0,0,0.55)',
        display: 'flex', alignItems: 'flex-start', justifyContent: 'center',
        paddingTop: '13vh',
      }}
      onClick={e => { if (e.target === e.currentTarget) onClose() }}
    >
      <div style={{
        width: 580,
        background: 'var(--color-bg-elevated, #25262b)',
        borderRadius: '12px',
        border: '1px solid var(--color-border, #373a40)',
        boxShadow: '0 12px 40px rgba(0,0,0,0.6)',
        overflow: 'hidden',
      }}>
        {/* Search input */}
        <div style={{ padding: '14px 16px', display: 'flex', alignItems: 'center', gap: '10px', borderBottom: '1px solid var(--color-border, #373a40)' }}>
          <span style={{ fontSize: '16px', color: 'var(--color-text-muted)', flexShrink: 0 }}>🔍</span>
          <input
            ref={inputRef}
            type="text"
            placeholder="搜尋功能或筆記…"
            value={query}
            onChange={e => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            style={{
              flex: 1, background: 'transparent', border: 'none', outline: 'none',
              color: 'var(--color-text-primary)', fontSize: '15px',
            }}
          />
          <kbd style={{
            fontSize: '11px', color: 'var(--color-text-muted)',
            border: '1px solid var(--color-border)', borderRadius: '4px',
            padding: '2px 6px', background: 'var(--color-bg-base)', flexShrink: 0,
          }}>ESC</kbd>
        </div>

        {/* Results */}
        <div style={{ maxHeight: '360px', overflowY: 'auto' }}>
          {results.length === 0 && (
            <div style={{ padding: '24px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: '13px' }}>
              找不到結果
            </div>
          )}
          {results.map((item, i) => {
            const isSelected = i === selectedIndex
            const showNoteSep = i === firstNoteIdx && firstNoteIdx > 0

            return (
              <div key={item.kind === 'feature' ? item.feature.id : item.path}>
                {showNoteSep && (
                  <div style={{ padding: '6px 14px 2px', fontSize: '11px', color: 'var(--color-text-muted)', fontWeight: 600, letterSpacing: '0.05em', textTransform: 'uppercase', borderTop: '1px solid var(--color-border)' }}>
                    筆記
                  </div>
                )}
                {i === 0 && item.kind === 'feature' && (
                  <div style={{ padding: '6px 14px 2px', fontSize: '11px', color: 'var(--color-text-muted)', fontWeight: 600, letterSpacing: '0.05em', textTransform: 'uppercase' }}>
                    功能
                  </div>
                )}
                <div
                  onClick={() => handleSelect(item)}
                  onMouseEnter={() => setSelectedIndex(i)}
                  style={{
                    padding: '9px 14px', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '10px',
                    background: isSelected ? 'var(--color-bg-hover, rgba(124,140,248,0.12))' : 'transparent',
                    borderLeft: isSelected ? '2px solid var(--color-accent, #7c8cf8)' : '2px solid transparent',
                  }}
                >
                  <span style={{ fontSize: '16px', width: '20px', textAlign: 'center', flexShrink: 0 }}>
                    {item.kind === 'feature' ? item.feature.icon : '📝'}
                  </span>
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={{ fontSize: '13px', color: 'var(--color-text-primary)', fontWeight: item.kind === 'feature' ? 500 : 400 }}>
                      {item.kind === 'feature' ? item.feature.label : item.title}
                    </div>
                    {item.kind === 'note' && (
                      <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '1px', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                        {item.path}
                      </div>
                    )}
                  </div>
                </div>
              </div>
            )
          })}
        </div>

        {/* Footer hint */}
        <div style={{ padding: '8px 14px', borderTop: '1px solid var(--color-border)', display: 'flex', gap: '16px', fontSize: '11px', color: 'var(--color-text-muted)' }}>
          <span><kbd style={{ padding: '1px 5px', border: '1px solid var(--color-border)', borderRadius: '3px', background: 'var(--color-bg-base)' }}>↑↓</kbd> 導覽</span>
          <span><kbd style={{ padding: '1px 5px', border: '1px solid var(--color-border)', borderRadius: '3px', background: 'var(--color-bg-base)' }}>↵</kbd> 開啟</span>
        </div>
      </div>
    </div>
  )
}
