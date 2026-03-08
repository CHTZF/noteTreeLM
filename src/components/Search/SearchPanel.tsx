import { useState, useRef, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { SearchResult } from '../../types/models'

interface SearchPanelProps {
  onOpenNote: (path: string) => void
}

// ── Icons ─────────────────────────────────────────────────────────────────────

const ListViewIcon = () => (
  <svg width="14" height="14" viewBox="0 0 14 14" fill="currentColor">
    <rect x="0" y="2" width="14" height="2" rx="1"/>
    <rect x="0" y="6" width="14" height="2" rx="1"/>
    <rect x="0" y="10" width="14" height="2" rx="1"/>
  </svg>
)

const TreeViewIcon = () => (
  <svg width="14" height="14" viewBox="0 0 14 14" fill="currentColor">
    <rect x="0" y="2" width="14" height="2" rx="1"/>
    <rect x="1" y="3.5" width="1.5" height="7" rx="0.75"/>
    <rect x="2" y="6" width="3" height="1.5" rx="0.75"/>
    <rect x="4.5" y="5.25" width="9.5" height="2" rx="1"/>
    <rect x="2" y="9.5" width="3" height="1.5" rx="0.75"/>
    <rect x="4.5" y="8.75" width="9.5" height="2" rx="1"/>
  </svg>
)

// ── Tree building ─────────────────────────────────────────────────────────────

interface TreeNode {
  name: string
  fullPath: string
  isFile: boolean
  result?: SearchResult
  children: TreeNode[]
}

function buildTree(results: SearchResult[]): TreeNode[] {
  const map = new Map<string, TreeNode>()
  const roots: TreeNode[] = []

  for (const r of results) {
    const parts = r.path.split('/')
    for (let i = 0; i < parts.length; i++) {
      const fullPath = parts.slice(0, i + 1).join('/')
      if (!map.has(fullPath)) {
        const node: TreeNode = {
          name: parts[i],
          fullPath,
          isFile: i === parts.length - 1,
          result: i === parts.length - 1 ? r : undefined,
          children: [],
        }
        map.set(fullPath, node)
        if (i === 0) {
          roots.push(node)
        } else {
          const parentPath = parts.slice(0, i).join('/')
          map.get(parentPath)!.children.push(node)
        }
      }
    }
  }

  function sort(nodes: TreeNode[]) {
    nodes.sort((a, b) => {
      if (a.isFile !== b.isFile) return a.isFile ? 1 : -1
      return a.name.localeCompare(b.name)
    })
    nodes.forEach(n => sort(n.children))
  }
  sort(roots)
  return roots
}

// ── Tree node component ───────────────────────────────────────────────────────

function TreeNodeView({ node, depth, onOpenNote }: {
  node: TreeNode
  depth: number
  onOpenNote: (path: string) => void
}) {
  const [open, setOpen] = useState(true)
  const pl = 8 + depth * 14

  if (node.isFile) {
    const displayName = node.name.replace(/\.(md|markdown|mdx)$/i, '')
    return (
      <div
        onClick={() => onOpenNote(node.fullPath)}
        style={{ paddingTop: 5, paddingBottom: 5, paddingRight: 12, paddingLeft: pl, cursor: 'pointer' }}
        onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
        onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
      >
        <div style={{ fontSize: '13px', color: 'var(--color-text-primary)', fontWeight: 500, display: 'flex', alignItems: 'center', gap: 4 }}>
          <span style={{ color: 'var(--color-text-muted)', fontSize: '11px', flexShrink: 0 }}>›</span>
          {displayName}
        </div>
        {node.result?.snippet && (
          <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginTop: 2, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', paddingLeft: 14 }}>
            {node.result.snippet}
          </div>
        )}
      </div>
    )
  }

  return (
    <div>
      <div
        onClick={() => setOpen(o => !o)}
        style={{ paddingTop: 4, paddingBottom: 4, paddingRight: 12, paddingLeft: pl, cursor: 'pointer', display: 'flex', alignItems: 'center', gap: 5 }}
        onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
        onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
      >
        <span style={{ fontSize: '9px', color: 'var(--color-text-muted)', flexShrink: 0, width: 8 }}>{open ? '▼' : '▶'}</span>
        <span style={{ fontSize: '12px', color: 'var(--color-text-muted)', fontWeight: 600 }}>{node.name}</span>
      </div>
      {open && node.children.map(child => (
        <TreeNodeView key={child.fullPath} node={child} depth={depth + 1} onOpenNote={onOpenNote} />
      ))}
    </div>
  )
}

// ── Main component ────────────────────────────────────────────────────────────

export default function SearchPanel({ onOpenNote }: SearchPanelProps) {
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<SearchResult[]>([])
  const [isSearching, setIsSearching] = useState(false)
  const [searchError, setSearchError] = useState<string | null>(null)
  const [viewMode, setViewMode] = useState<'list' | 'tree'>('list')
  const debounceRef = useRef<ReturnType<typeof setTimeout>>()

  const doSearch = useCallback(async (q: string) => {
    if (!q.trim()) { setResults([]); setIsSearching(false); setSearchError(null); return }
    setIsSearching(true)
    setSearchError(null)
    try {
      const timeout = new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error('搜尋超時，請稍後再試')), 8000)
      )
      const res = await Promise.race([
        invoke<SearchResult[]>('search', { query: q }),
        timeout,
      ])
      setResults(res)
    } catch (e: any) {
      setSearchError(String(e?.message ?? e))
      setResults([])
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

  const treeRoots = viewMode === 'tree' ? buildTree(results) : []

  const btnStyle = (active: boolean): React.CSSProperties => ({
    display: 'flex', alignItems: 'center', justifyContent: 'center',
    width: 26, height: 26, borderRadius: 5, cursor: 'pointer', flexShrink: 0,
    background: active ? 'var(--color-bg-elevated)' : 'transparent',
    color: active ? 'var(--color-text-primary)' : 'var(--color-text-muted)',
    border: active ? '1px solid var(--color-border)' : '1px solid transparent',
  })

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      {/* Search input + view toggle */}
      <div style={{ padding: '8px', display: 'flex', gap: 6, alignItems: 'center' }}>
        <input
          type="text"
          placeholder="搜索筆記…"
          value={query}
          onChange={(e) => handleChange(e.target.value)}
          style={{
            flex: 1, padding: '6px 10px', boxSizing: 'border-box',
            background: 'var(--color-bg-elevated)', borderRadius: '6px',
            border: '1px solid var(--color-border)',
            color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none',
          }}
        />
        <button style={btnStyle(viewMode === 'list')} title="條列顯示" onClick={() => setViewMode('list')}>
          <ListViewIcon />
        </button>
        <button style={btnStyle(viewMode === 'tree')} title="樹狀顯示" onClick={() => setViewMode('tree')}>
          <TreeViewIcon />
        </button>
      </div>

      {/* Results */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '4px 0' }}>
        {isSearching && (
          <div style={{ padding: '16px', color: 'var(--color-text-secondary)', fontSize: '13px', textAlign: 'center' }}>搜索中…</div>
        )}
        {!isSearching && searchError && (
          <div style={{ padding: '16px', color: 'var(--color-error, #ef4444)', fontSize: '12px' }}>{searchError}</div>
        )}
        {!isSearching && !searchError && results.length === 0 && query && (
          <div style={{ padding: '16px', color: 'var(--color-text-muted)', fontSize: '13px', textAlign: 'center' }}>無結果</div>
        )}

        {viewMode === 'list' && results.map((r) => (
          <div
            key={r.path}
            onClick={() => onOpenNote(r.path)}
            style={{ padding: '8px 12px', cursor: 'pointer', borderBottom: '1px solid var(--color-border-subtle)' }}
            onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
            onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
          >
            <div style={{ fontSize: '13px', color: 'var(--color-text-primary)', fontWeight: 500 }}>{r.title}</div>
            <div style={{ fontSize: '12px', color: 'var(--color-text-muted)', marginTop: '3px', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{r.snippet}</div>
          </div>
        ))}

        {viewMode === 'tree' && treeRoots.map(node => (
          <TreeNodeView key={node.fullPath} node={node} depth={0} onOpenNote={onOpenNote} />
        ))}
      </div>
    </div>
  )
}
