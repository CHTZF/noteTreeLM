import { useEffect, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { useEditorStore } from '../../stores/editorStore'
import { Link } from '../../types/models'
import { useVaultStore } from '../../stores/vaultStore'

interface BacklinksPanelProps {
  onOpenNote: (path: string) => void
}

export default function BacklinksPanel({ onOpenNote }: BacklinksPanelProps) {
  const { currentPath, viewMode } = useEditorStore()
  const { notes } = useVaultStore()
  const [backlinks, setBacklinks] = useState<Link[]>([])
  const [isExpanded, setIsExpanded] = useState(false)

  const currentTitle = notes.find((n) => n.path === currentPath)?.title || ''

  // live 模式隱藏；切換到 preview 預設收起，切換到 editor/source 預設展開
  useEffect(() => {
    if (viewMode === 'preview') setIsExpanded(false)
    else if (viewMode === 'editor') setIsExpanded(true)
  }, [viewMode])

  useEffect(() => {
    if (!currentTitle) { setBacklinks([]); return }
    invoke<Link[]>('get_backlinks', { title: currentTitle }).then(setBacklinks)
  }, [currentTitle])

  if (!currentPath || viewMode === 'live') return null

  return (
    <div style={{ borderTop: '1px solid var(--color-border)', background: 'var(--color-bg-base)', flexShrink: 0 }}>
      <div
        onClick={() => setIsExpanded(!isExpanded)}
        style={{
          display: 'flex', alignItems: 'center', justifyContent: 'space-between',
          padding: '8px 16px', cursor: 'pointer',
          fontSize: '12px', color: 'var(--color-text-secondary)',
        }}
      >
        <span>反向連結 ({backlinks.length})</span>
        <span style={{ fontSize: '10px' }}>{isExpanded ? '▼' : '▶'}</span>
      </div>
      {isExpanded && backlinks.length > 0 && (
        <div style={{ maxHeight: '160px', overflowY: 'auto', padding: '0 0 8px' }}>
          {backlinks.map((link) => (
            <div
              key={link.id}
              onClick={() => onOpenNote(link.source_path)}
              style={{ padding: '4px 16px', fontSize: '13px', color: 'var(--color-text-secondary)', cursor: 'pointer', transition: 'background 0.1s' }}
              onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
              onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
            >
              📝 <span style={{ color: 'var(--color-accent)' }}>{link.source_path}</span>
              <span style={{ color: 'var(--color-text-muted)', fontSize: '12px', marginLeft: '8px' }}>
                第 {link.line_number + 1} 行
              </span>
            </div>
          ))}
        </div>
      )}
      {isExpanded && backlinks.length === 0 && (
        <div style={{ padding: '4px 16px 8px', fontSize: '12px', color: 'var(--color-text-muted)' }}>
          沒有其他筆記連結到這裡
        </div>
      )}
    </div>
  )
}
