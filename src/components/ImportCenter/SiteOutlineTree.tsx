import { useState } from 'react'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import {
  faChevronRight, faChevronDown, faCircleCheck, faClock,
  faSpinner, faTriangleExclamation, faArrowsRotate, faFileImport,
} from '@fortawesome/free-solid-svg-icons'

export interface ImportPage {
  page_id: string
  session_id: string
  url: string
  title: string
  parent_url: string | null
  depth: number
  note_path: string | null
  content_hash: string | null
  status: string // pending | importing | imported | updated | failed
  last_crawled: number | null
}

interface TreeNode {
  page: ImportPage
  children: TreeNode[]
}

function buildTree(pages: ImportPage[]): TreeNode[] {
  const byUrl = new Map<string, TreeNode>()
  for (const p of pages) {
    byUrl.set(p.url, { page: p, children: [] })
  }
  const roots: TreeNode[] = []
  for (const node of byUrl.values()) {
    const parentUrl = node.page.parent_url
    if (parentUrl && byUrl.has(parentUrl)) {
      byUrl.get(parentUrl)!.children.push(node)
    } else {
      roots.push(node)
    }
  }
  return roots
}

function statusIcon(status: string) {
  switch (status) {
    case 'imported': return <FontAwesomeIcon icon={faCircleCheck} style={{ color: 'var(--color-success, #4caf50)', fontSize: 11 }} />
    case 'importing': return <FontAwesomeIcon icon={faSpinner} spin style={{ color: 'var(--color-accent)', fontSize: 11 }} />
    case 'updated': return <FontAwesomeIcon icon={faArrowsRotate} style={{ color: 'var(--color-warning, #ff9800)', fontSize: 11 }} />
    case 'failed': return <FontAwesomeIcon icon={faTriangleExclamation} style={{ color: 'var(--color-error, #f44336)', fontSize: 11 }} />
    default: return <FontAwesomeIcon icon={faClock} style={{ color: 'var(--color-text-muted)', fontSize: 11 }} />
  }
}

interface TreeNodeRowProps {
  node: TreeNode
  importingIds: Set<string>
  selectedId: string | null
  onSelect: (page: ImportPage) => void
  onImport: (page: ImportPage) => void
  onOpenNote: (path: string) => void
}

function TreeNodeRow({ node, importingIds, selectedId, onSelect, onImport, onOpenNote }: TreeNodeRowProps) {
  const [expanded, setExpanded] = useState(true)
  const hasChildren = node.children.length > 0
  const isImporting = importingIds.has(node.page.page_id)
  const status = isImporting ? 'importing' : node.page.status
  const isSelected = selectedId === node.page.page_id

  return (
    <div>
      <div
        onClick={() => onSelect(node.page)}
        style={{
          display: 'flex', alignItems: 'center', gap: 4,
          padding: '3px 4px 3px ' + (node.page.depth * 14 + 4) + 'px',
          cursor: 'pointer', borderRadius: 4,
          background: isSelected ? 'var(--color-accent-muted, rgba(99,102,241,0.15))' : 'transparent',
          fontSize: 12,
          color: 'var(--color-text-primary)',
        }}
        onMouseEnter={e => { if (!isSelected) (e.currentTarget as HTMLDivElement).style.background = 'var(--color-hover)' }}
        onMouseLeave={e => { if (!isSelected) (e.currentTarget as HTMLDivElement).style.background = 'transparent' }}
      >
        {hasChildren ? (
          <span
            onClick={e => { e.stopPropagation(); setExpanded(v => !v) }}
            style={{ width: 14, cursor: 'pointer', flexShrink: 0, color: 'var(--color-text-muted)' }}
          >
            <FontAwesomeIcon icon={expanded ? faChevronDown : faChevronRight} style={{ fontSize: 9 }} />
          </span>
        ) : (
          <span style={{ width: 14, flexShrink: 0 }} />
        )}
        <span style={{ flexShrink: 0 }}>{statusIcon(status)}</span>
        <span style={{
          flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          color: status === 'pending' ? 'var(--color-text-muted)' : 'var(--color-text-primary)',
        }}>
          {node.page.title || new URL(node.page.url).pathname || node.page.url}
        </span>
        <span style={{ display: 'flex', gap: 4, flexShrink: 0 }}>
          {node.page.status === 'imported' || node.page.status === 'updated' ? (
            <button
              title="開啟筆記"
              onClick={e => { e.stopPropagation(); node.page.note_path && onOpenNote(node.page.note_path) }}
              style={btnStyle}
            >↗</button>
          ) : null}
          {(node.page.status === 'pending' || node.page.status === 'failed') && !isImporting && (
            <button
              title="匯入此頁"
              onClick={e => { e.stopPropagation(); onImport(node.page) }}
              style={btnStyle}
            ><FontAwesomeIcon icon={faFileImport} style={{ fontSize: 9 }} /></button>
          )}
        </span>
      </div>
      {hasChildren && expanded && node.children.map(child => (
        <TreeNodeRow
          key={child.page.page_id}
          node={child}
          importingIds={importingIds}
          selectedId={selectedId}
          onSelect={onSelect}
          onImport={onImport}
          onOpenNote={onOpenNote}
        />
      ))}
    </div>
  )
}

const btnStyle: React.CSSProperties = {
  background: 'none', border: 'none', cursor: 'pointer',
  color: 'var(--color-text-muted)', padding: '1px 4px',
  borderRadius: 3, fontSize: 10,
}

interface Props {
  pages: ImportPage[]
  importingIds: Set<string>
  selectedId: string | null
  onSelect: (page: ImportPage) => void
  onImport: (page: ImportPage) => void
  onOpenNote: (path: string) => void
}

export default function SiteOutlineTree({ pages, importingIds, selectedId, onSelect, onImport, onOpenNote }: Props) {
  if (pages.length === 0) {
    return (
      <div style={{ padding: '20px 12px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: 12 }}>
        尚無頁面，請先點擊「分析網站」
      </div>
    )
  }

  const roots = buildTree(pages)

  return (
    <div style={{ overflowY: 'auto', flex: 1 }}>
      {roots.map(node => (
        <TreeNodeRow
          key={node.page.page_id}
          node={node}
          importingIds={importingIds}
          selectedId={selectedId}
          onSelect={onSelect}
          onImport={onImport}
          onOpenNote={onOpenNote}
        />
      ))}
    </div>
  )
}
