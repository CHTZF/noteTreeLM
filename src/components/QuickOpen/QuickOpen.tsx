import { useState, useEffect, useRef } from 'react'
import { useVaultStore } from '../../stores/vaultStore'

interface QuickOpenProps {
  onSelect: (path: string) => void
  onClose: () => void
}

export default function QuickOpen({ onSelect, onClose }: QuickOpenProps) {
  const [query, setQuery] = useState('')
  const [selectedIndex, setSelectedIndex] = useState(0)
  const { notes } = useVaultStore()
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => { inputRef.current?.focus() }, [])

  const filtered = query
    ? notes.filter((n) => n.title.toLowerCase().includes(query.toLowerCase())).slice(0, 10)
    : notes.slice(0, 10)

  useEffect(() => { setSelectedIndex(0) }, [query])

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); setSelectedIndex((i) => Math.min(i + 1, filtered.length - 1)) }
    if (e.key === 'ArrowUp') { e.preventDefault(); setSelectedIndex((i) => Math.max(i - 1, 0)) }
    if (e.key === 'Enter' && filtered[selectedIndex]) { onSelect(filtered[selectedIndex].path) }
    if (e.key === 'Escape') { onClose() }
  }

  return (
    <div
      style={{
        position: 'fixed', inset: 0, zIndex: 2000,
        background: 'rgba(0,0,0,0.6)',
        display: 'flex', alignItems: 'flex-start', justifyContent: 'center',
        paddingTop: '15vh',
      }}
      onClick={(e) => { if (e.target === e.currentTarget) onClose() }}
    >
      <div style={{
        width: 560, background: '#25262b', borderRadius: '10px',
        border: '1px solid #373a40', boxShadow: '0 8px 32px rgba(0,0,0,0.6)',
        overflow: 'hidden',
      }}>
        <div style={{ padding: '12px 16px', borderBottom: '1px solid #373a40' }}>
          <input
            ref={inputRef}
            type="text"
            placeholder="搜尋筆記…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            style={{
              width: '100%', background: 'transparent',
              color: '#c9cdd4', fontSize: '15px',
            }}
          />
        </div>
        <div style={{ maxHeight: '320px', overflowY: 'auto' }}>
          {filtered.map((note, i) => (
            <div
              key={note.path}
              onClick={() => onSelect(note.path)}
              style={{
                padding: '10px 16px', cursor: 'pointer',
                background: i === selectedIndex ? 'rgba(124,140,248,0.15)' : 'transparent',
                display: 'flex', alignItems: 'center', gap: '8px',
              }}
              onMouseEnter={() => setSelectedIndex(i)}
            >
              <span style={{ fontSize: '13px' }}>📝</span>
              <div>
                <div style={{ fontSize: '14px', color: '#c9cdd4' }}>{note.title}</div>
                <div style={{ fontSize: '12px', color: '#5c6070' }}>{note.path}</div>
              </div>
            </div>
          ))}
          {filtered.length === 0 && (
            <div style={{ padding: '20px', textAlign: 'center', color: '#5c6070', fontSize: '13px' }}>
              找不到筆記
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
