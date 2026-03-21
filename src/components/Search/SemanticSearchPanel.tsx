import { useState, useRef, useCallback, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'


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
  method: string   // 'vector' | 'bm25' | 'contains' | 'no_index' | 'no_results'
}

interface IndexStats {
  total: number
  chunked: number
  embedded: number
}

interface ReindexProgress {
  done: number
  total: number
  path: string
}

// ── Parse the formatted result from tool_search_vault ─────────────────────
const METHOD_LABEL: Record<string, string> = {
  vector:       '向量搜尋',
  bm25:         'BM25 全文',
  contains:     '字串包含',
  no_index:     '尚未建立索引',
  no_results:   '無結果',
  vector_fail:  '向量連線失敗',
  no_vec_index: '尚未建立向量索引',
}

function parseResult(raw: string): SearchResult {
  const direct: ChunkResult[] = []
  const expanded: ExpandedLink[] = []
  let inExpanded = false
  let method = 'unknown'

  for (const line of raw.split('\n')) {
    if (line.startsWith('🔍 ')) { method = line.slice(3).trim(); continue }
    if (line.startsWith('📎')) { inExpanded = true; continue }
    if (!line.startsWith('- ')) continue

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
  return { direct, expanded, rawText: raw, method }
}

function VectorFailDiag() {
  const [diag, setDiag] = useState<string | null>(null)
  const [checking, setChecking] = useState(false)
  return (
    <div>
      <div>Embedding Server 連線失敗，請檢查伺服器狀態</div>
      <button
        onClick={async () => {
          setChecking(true)
          try {
            const r = await invoke<string>('check_embedding_endpoint')
            setDiag(r)
          } catch (e: any) {
            setDiag(String(e))
          } finally {
            setChecking(false)
          }
        }}
        style={{ marginTop: '6px', fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
          border: '1px solid var(--color-border)', background: 'transparent',
          color: 'var(--color-text-muted)', cursor: 'pointer' }}
      >
        {checking ? '診斷中…' : '診斷 Endpoint'}
      </button>
      {diag && (
        <pre style={{ marginTop: '6px', fontSize: '10px', color: 'var(--color-text-secondary)',
          whiteSpace: 'pre-wrap', wordBreak: 'break-all', background: 'var(--color-bg-base)',
          padding: '6px', borderRadius: '4px', border: '1px solid var(--color-border)' }}>
          {diag}
        </pre>
      )}
    </div>
  )
}

export default function SemanticSearchPanel({ onOpenNote }: Props) {
  const [query, setQuery]           = useState('')
  const [result, setResult]         = useState<SearchResult | null>(null)
  const [loading, setLoading]       = useState(false)
  const [indexing, setIndexing]     = useState(false)
  const [stats, setStats]           = useState<IndexStats | null>(null)
  const [progress, setProgress]     = useState<ReindexProgress | null>(null)
  const [error, setError]           = useState<string | null>(null)
  const [verifiedOnly, setVerifiedOnly] = useState(false)
  const inputRef                    = useRef<HTMLInputElement>(null)
  const unlistenRef                 = useRef<UnlistenFn | null>(null)

  useEffect(() => { inputRef.current?.focus() }, [])

  const loadStats = useCallback(async () => {
    try {
      const s = await invoke<IndexStats>('get_index_stats')
      setStats(s)
    } catch {}
  }, [])

  useEffect(() => { loadStats() }, [loadStats])

  const handleSearch = useCallback(async () => {
    const q = query.trim()
    if (!q) return
    setLoading(true); setError(null)
    try {
      const raw = await invoke<string>('search_vault_chunks', { query: q, verifiedOnly: verifiedOnly || undefined })
      setResult(parseResult(raw))
    } catch (e: any) {
      setError(typeof e === 'string' ? e : '搜尋失敗')
    } finally {
      setLoading(false)
    }
  }, [query])


  const handleReindex = async () => {
    setIndexing(true); setError(null); setProgress(null)

    // 監聽進度事件
    unlistenRef.current?.()
    unlistenRef.current = await listen<ReindexProgress>('reindex:progress', (e) => {
      setProgress(e.payload)
    })

    try {
      await invoke<number>('reindex_vault_chunks')
      await loadStats()
    } catch (e: any) {
      setError(typeof e === 'string' ? e : '建立索引失敗')
    } finally {
      unlistenRef.current?.()
      unlistenRef.current = null
      setIndexing(false)
      setProgress(null)
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

  const pct = progress && progress.total > 0
    ? Math.round(progress.done / progress.total * 100)
    : 0

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
          <div style={{ display: 'flex', gap: '4px' }}>
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
              {indexing ? '索引中…' : '重建索引'}
            </button>
          </div>
        </div>

        {/* Stats row */}
        <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
          {stats && !indexing && (
            <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>
              {stats.embedded > 0
                ? <>向量 <strong style={{ color: 'var(--color-accent)' }}>{stats.embedded}</strong> / {stats.total} 篇</>
                : <>已索引 <strong style={{ color: 'var(--color-text-secondary)' }}>{stats.chunked}</strong> / {stats.total} 篇</>
              }
            </span>
          )}
          {indexing && progress && (
            <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', flex: 1, minWidth: 0 }}>
              {progress.done} / {progress.total}
              {progress.path && (
                <span style={{ marginLeft: '6px', color: 'var(--color-text-muted)', opacity: 0.7,
                  overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
                  display: 'inline-block', maxWidth: '140px', verticalAlign: 'bottom' }}>
                  {progress.path.split('/').pop()}
                </span>
              )}
            </span>
          )}
        </div>

        {/* Progress bar (shown during reindex) */}
        {indexing && (
          <div style={{ height: '3px', borderRadius: '2px', background: 'var(--color-border)', overflow: 'hidden' }}>
            <div style={{
              height: '100%',
              width: progress ? `${pct}%` : '0%',
              background: 'var(--color-accent)',
              borderRadius: '2px',
              transition: 'width 0.2s ease',
            }} />
          </div>
        )}

        {/* Verified-only toggle */}
        <label style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer', userSelect: 'none' }}>
          <input
            type="checkbox"
            checked={verifiedOnly}
            onChange={e => setVerifiedOnly(e.target.checked)}
            style={{ accentColor: 'var(--color-accent)', cursor: 'pointer' }}
          />
          <span style={{ fontSize: '11px', color: verifiedOnly ? 'var(--color-accent)' : 'var(--color-text-muted)' }}>
            僅已驗證
          </span>
        </label>

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

        {result && (
          <div style={{ padding: '6px 12px 0', display: 'flex', alignItems: 'center', gap: '6px' }}>
            <span style={{
              fontSize: '10px', padding: '1px 6px', borderRadius: '8px',
              background: result.method === 'vector'
                ? 'color-mix(in srgb, var(--color-accent) 15%, transparent)'
                : result.method === 'bm25'
                ? 'color-mix(in srgb, var(--color-success) 15%, transparent)'
                : result.method === 'no_index' || result.method === 'no_results' || result.method === 'no_vec_index' || result.method === 'vector_fail'
                ? 'color-mix(in srgb, var(--color-warning) 15%, transparent)'
                : 'color-mix(in srgb, var(--color-text-muted) 15%, transparent)',
              color: result.method === 'vector'
                ? 'var(--color-accent)'
                : result.method === 'bm25'
                ? 'var(--color-success)'
                : result.method === 'no_index' || result.method === 'no_results' || result.method === 'no_vec_index' || result.method === 'vector_fail'
                ? 'var(--color-warning)'
                : 'var(--color-text-muted)',
              border: '1px solid currentColor',
              fontWeight: 500,
            }}>
              {METHOD_LABEL[result.method] ?? result.method}
            </span>
          </div>
        )}

        {result && result.direct.length === 0 && result.expanded.length === 0 && (
          <div style={{ padding: '20px 12px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: '13px' }}>
            {result.method === 'no_index'
              ? '尚未建立索引，請點上方「重建索引」'
              : result.method === 'no_vec_index'
              ? '尚未建立向量索引，請確認 Embedding Server 已啟動後重建索引'
              : result.method === 'vector_fail'
              ? <VectorFailDiag />
              : '未找到相關段落'}
          </div>
        )}

        {result && result.direct.length > 0 && (
          <>
            {result.direct.map((item, i) => (
              <div
                key={i}
                onClick={() => onOpenNote(item.section ? `${item.path}#${item.section}` : item.path)}
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
