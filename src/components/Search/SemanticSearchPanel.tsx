import { useState, useRef, useCallback, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'

interface Props {
  onOpenNote: (path: string) => void
}

interface ChunkResult {
  path: string
  title: string
  section: string
  snippet: string
  isExpanded?: boolean
}

interface ExpandedLink {
  path: string
  title: string
}

interface SearchResult {
  direct: ChunkResult[]
  expanded: ExpandedLink[]
  rawText: string
}

// ── Parse the formatted result from tool_search_vault ─────────────────────
function parseResult(raw: string): SearchResult {
  const direct: ChunkResult[] = []
  const expanded: ExpandedLink[] = []
  let inExpanded = false

  for (const line of raw.split('\n')) {
    if (line.startsWith('📎')) { inExpanded = true; continue }
    if (!line.startsWith('- ')) continue

    // Match: - **title § section** (path)\n  snippet
    const m = line.match(/^- \*\*(.+?)\*\* \(([^)]+)\)/)
    if (!m) continue
    const [, titleSection, path] = m

    if (inExpanded) {
      expanded.push({ path, title: titleSection })
    } else {
      const sepIdx = titleSection.indexOf(' § ')
      const title   = sepIdx >= 0 ? titleSection.slice(0, sepIdx) : titleSection
      const section = sepIdx >= 0 ? titleSection.slice(sepIdx + 3) : ''
      const snippetStart = line.indexOf('\n  ')
      const snippet = snippetStart >= 0 ? line.slice(snippetStart + 3) : ''
      direct.push({ path, title, section, snippet })
    }
  }
  return { direct, expanded, rawText: raw }
}

export default function SemanticSearchPanel({ onOpenNote }: Props) {
  const [query, setQuery]           = useState('')
  const [result, setResult]         = useState<SearchResult | null>(null)
  const [loading, setLoading]       = useState(false)
  const [indexing, setIndexing]     = useState(false)
  const [indexed, setIndexed]       = useState<number | null>(null)
  const [error, setError]           = useState<string | null>(null)
  const inputRef                    = useRef<HTMLInputElement>(null)

  useEffect(() => { inputRef.current?.focus() }, [])

  const handleSearch = useCallback(async () => {
    const q = query.trim()
    if (!q) return
    setLoading(true); setError(null)
    try {
      // invoke search_vault directly (chunk-aware)
      const raw = await invoke<string>('search_vault_chunks', { query: q })
      setResult(parseResult(raw))
    } catch (e: any) {
      setError(typeof e === 'string' ? e : '搜尋失敗')
    } finally {
      setLoading(false)
    }
  }, [query])

  const handleReindex = async () => {
    setIndexing(true); setError(null)
    try {
      const count = await invoke<number>('reindex_vault_chunks')
      setIndexed(count)
    } catch (e: any) {
      setError(typeof e === 'string' ? e : '建立索引失敗')
    } finally {
      setIndexing(false)
    }
  }

  const inputStyle: React.CSSProperties = {
    flex: 1,
    padding: '6px 10px',
    background: 'var(--color-bg-base)',
    border: '1px solid var(--color-border)',
    borderRadius: '6px',
    color: 'var(--color-text-primary)',
    fontSize: '13px',
    outline: 'none',
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', overflow: 'hidden' }}>
      {/* Header */}
      <div style={{
        padding: '10px 12px 8px',
        borderBottom: '1px solid var(--color-border)',
        display: 'flex', flexDirection: 'column', gap: '8px',
      }}>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
          <span style={{ fontSize: '11px', fontWeight: 600, letterSpacing: '0.07em', color: 'var(--color-text-secondary)', textTransform: 'uppercase' }}>
            語意搜尋
          </span>
          <button
            onClick={handleReindex}
            disabled={indexing}
            title="重建 chunk 索引（首次使用或新增大量筆記後）"
            style={{
              fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
              border: '1px solid var(--color-border)',
              background: 'transparent', color: 'var(--color-text-muted)',
              cursor: indexing ? 'wait' : 'pointer',
              opacity: indexing ? 0.5 : 1,
            }}
          >
            {indexing ? '建立中…' : '重建索引'}
          </button>
        </div>

        {indexed !== null && (
          <div style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>
            ✅ 已索引 {indexed} 篇筆記
          </div>
        )}

        {/* Search input */}
        <div style={{ display: 'flex', gap: '6px' }}>
          <input
            ref={inputRef}
            value={query}
            onChange={e => setQuery(e.target.value)}
            onKeyDown={e => { if (e.key === 'Enter') handleSearch() }}
            placeholder="輸入關鍵字搜尋…"
            style={inputStyle}
          />
          <button
            onClick={handleSearch}
            disabled={loading || !query.trim()}
            title="搜尋"
            style={{
              padding: '6px 10px', borderRadius: '6px', border: 'none',
              background: 'var(--color-accent)', color: '#fff',
              fontSize: '13px', cursor: loading ? 'wait' : 'pointer',
              opacity: (loading || !query.trim()) ? 0.5 : 1,
              display: 'flex', alignItems: 'center', justifyContent: 'center',
            }}
          >
            {loading ? (
              <span style={{ fontSize: '12px', lineHeight: 1 }}>…</span>
            ) : (
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                <circle cx="11" cy="11" r="8"/>
                <line x1="21" y1="21" x2="16.65" y2="16.65"/>
              </svg>
            )}
          </button>
        </div>
      </div>

      {/* Results */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '8px 0' }}>
        {error && (
          <div style={{ padding: '8px 12px', color: 'var(--color-error, #e04040)', fontSize: '12px' }}>
            {error}
          </div>
        )}

        {result && result.direct.length === 0 && result.expanded.length === 0 && (
          <div style={{ padding: '20px 12px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: '13px' }}>
            未找到相關段落
          </div>
        )}

        {result && result.direct.length > 0 && (
          <>
            {result.direct.map((item, i) => (
              <div
                key={i}
                onClick={() => onOpenNote(item.path)}
                style={{
                  padding: '8px 12px',
                  cursor: 'pointer',
                  borderBottom: '1px solid var(--color-border)',
                }}
                onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
                onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
              >
                <div style={{ display: 'flex', alignItems: 'baseline', gap: '6px', marginBottom: '2px' }}>
                  <span style={{ fontSize: '13px', fontWeight: 600, color: 'var(--color-text-primary)' }}>
                    {item.title}
                  </span>
                  {item.section && (
                    <span style={{ fontSize: '11px', color: 'var(--color-accent)' }}>
                      § {item.section}
                    </span>
                  )}
                </div>
                <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginBottom: '4px' }}>
                  {item.path}
                </div>
                {item.snippet && (
                  <div style={{
                    fontSize: '12px', color: 'var(--color-text-secondary)',
                    lineHeight: 1.5, display: '-webkit-box',
                    WebkitLineClamp: 3, WebkitBoxOrient: 'vertical', overflow: 'hidden',
                  }}>
                    {item.snippet}
                  </div>
                )}
              </div>
            ))}

            {result.expanded.length > 0 && (
              <>
                <div style={{ padding: '8px 12px 4px', fontSize: '11px', fontWeight: 600, color: 'var(--color-text-muted)', letterSpacing: '0.05em' }}>
                  📎 相關連結筆記
                </div>
                {result.expanded.map((item, i) => (
                  <div
                    key={i}
                    onClick={() => onOpenNote(item.path)}
                    style={{ padding: '6px 12px', cursor: 'pointer' }}
                    onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
                    onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
                  >
                    <div style={{ fontSize: '13px', color: 'var(--color-text-primary)' }}>{item.title}</div>
                    <div style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>{item.path}</div>
                  </div>
                ))}
              </>
            )}
          </>
        )}

        {!result && !loading && !error && (
          <div style={{ padding: '20px 12px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: '12px', lineHeight: 1.8 }}>
            <div>依段落語意搜尋筆記</div>
            <div style={{ marginTop: '4px' }}>首次使用請點「重建索引」</div>
          </div>
        )}
      </div>
    </div>
  )
}
