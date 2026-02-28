import { useState, useRef, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { SearchResult } from '../../types/models'

interface SearchPanelProps {
  onOpenNote: (path: string) => void
}

export default function SearchPanel({ onOpenNote }: SearchPanelProps) {
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<SearchResult[]>([])
  const [isSearching, setIsSearching] = useState(false)
  const debounceRef = useRef<ReturnType<typeof setTimeout>>()

  const doSearch = useCallback(async (q: string) => {
    if (!q.trim()) { setResults([]); setIsSearching(false); return }
    setIsSearching(true)
    try {
      const res = await invoke<SearchResult[]>('search', { query: q })
      setResults(res)
    } finally {
      setIsSearching(false)
    }
  }, [])

  const handleChange = (q: string) => {
    setQuery(q)
    if (debounceRef.current) clearTimeout(debounceRef.current)
    if (!q.trim()) { setResults([]); setIsSearching(false); return }
    setIsSearching(true)
    debounceRef.current = setTimeout(() => doSearch(q), 300)
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      <div style={{ padding: '8px' }}>
        <input
          type="text"
          placeholder="搜索筆記…"
          value={query}
          onChange={(e) => handleChange(e.target.value)}
          style={{
            width: '100%', padding: '6px 10px', boxSizing: 'border-box',
            background: 'var(--color-bg-elevated)', borderRadius: '6px',
            border: '1px solid var(--color-border)',
            color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none',
          }}
        />
      </div>
      <div style={{ flex: 1, overflowY: 'auto', padding: '4px 0' }}>
        {isSearching && (
          <div style={{ padding: '16px', color: 'var(--color-text-secondary)', fontSize: '13px', textAlign: 'center' }}>搜索中…</div>
        )}
        {!isSearching && results.length === 0 && query && (
          <div style={{ padding: '16px', color: 'var(--color-text-muted)', fontSize: '13px', textAlign: 'center' }}>無結果</div>
        )}
        {results.map((r) => (
          <div
            key={r.path}
            onClick={() => onOpenNote(r.path)}
            style={{
              padding: '8px 12px', cursor: 'pointer',
              borderBottom: '1px solid var(--color-border-subtle)',
            }}
            onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
            onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
          >
            <div style={{ fontSize: '13px', color: 'var(--color-text-primary)', fontWeight: 500 }}>{r.title}</div>
            <div style={{ fontSize: '12px', color: 'var(--color-text-muted)', marginTop: '3px', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{r.snippet}</div>
          </div>
        ))}
      </div>
    </div>
  )
}
