import { invoke } from '@tauri-apps/api/core'
import { useCallback, useEffect, useState } from 'react'
import { useEditorStore } from '../../stores/editorStore'

type NoteStatus = 'draft' | 'verified' | 'deprecated'

function parseFrontmatterStatus(content: string): NoteStatus | null {
  if (!content.startsWith('---')) return null
  const after = content.startsWith('---\r\n') ? 5 : 4
  const end = content.indexOf('\n---', after)
  if (end === -1) return null
  const fm = content.slice(after, end)
  for (const line of fm.split('\n')) {
    const trimmed = line.trimStart()
    if (trimmed.startsWith('status:')) {
      const val = trimmed.slice('status:'.length).trim()
      if (val === 'verified' || val === 'draft' || val === 'deprecated') return val
    }
  }
  return null
}

const STATUS_CONFIG: Record<NoteStatus, { label: string; color: string; bg: string }> = {
  draft:      { label: 'AI 草稿', color: 'var(--color-warning)',  bg: 'color-mix(in srgb, var(--color-warning) 12%, transparent)' },
  verified:   { label: '已驗證',  color: 'var(--color-success)',  bg: 'color-mix(in srgb, var(--color-success) 12%, transparent)' },
  deprecated: { label: '已棄用',  color: 'var(--color-text-muted)', bg: 'color-mix(in srgb, var(--color-text-muted) 12%, transparent)' },
}

export default function NoteStatusBadge() {
  const { currentPath, content } = useEditorStore()
  const [loading, setLoading] = useState(false)
  const [localStatus, setLocalStatus] = useState<NoteStatus | null>(null)

  // Parse status from content whenever currentPath or content changes
  useEffect(() => {
    setLocalStatus(parseFrontmatterStatus(content))
  }, [currentPath, content])

  const handleSetStatus = useCallback(async (newStatus: NoteStatus) => {
    if (!currentPath) return
    setLoading(true)
    try {
      await invoke('set_note_status', { path: currentPath, status: newStatus })
      setLocalStatus(newStatus)
      // Sync updated frontmatter back to CM6 editor without triggering auto-save
      const store = useEditorStore.getState()
      store.applyExternalWrite(setFrontmatterKey(store.content, 'status', newStatus))
    } catch (e) {
      console.error('[NoteStatusBadge] set_note_status error:', e)
    } finally {
      setLoading(false)
    }
  }, [currentPath])

  // Only show for .md files
  if (!currentPath || !/\.(md|markdown|mdx)$/i.test(currentPath)) return null

  // No status yet — show a compact dropdown to set one
  if (!localStatus) {
    return (
      <select
        disabled={loading}
        value=""
        onChange={e => { if (e.target.value) handleSetStatus(e.target.value as NoteStatus) }}
        title="設定知識狀態，讓知識庫助手可以使用此筆記"
        style={{
          fontSize: '11px', padding: '2px 6px', borderRadius: '10px',
          color: 'var(--color-text-muted)',
          background: 'transparent',
          border: '1px dashed var(--color-border)',
          cursor: 'pointer', opacity: loading ? 0.6 : 1,
        }}
      >
        <option value="">+ 知識狀態</option>
        <option value="draft">草稿</option>
        <option value="verified">已驗證</option>
        <option value="deprecated">已棄用</option>
      </select>
    )
  }

  const cfg = STATUS_CONFIG[localStatus]

  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: '4px', flexShrink: 0 }}>
      <span style={{
        fontSize: '11px', padding: '2px 7px', borderRadius: '10px',
        color: cfg.color, background: cfg.bg,
        border: `1px solid ${cfg.color}`,
        fontWeight: 500, letterSpacing: '0.02em', whiteSpace: 'nowrap',
      }}>
        {cfg.label}
      </span>

      {localStatus === 'draft' && (
        <button
          disabled={loading}
          onClick={() => handleSetStatus('verified')}
          title="標記為已驗證，知識庫助手可使用此筆記"
          style={{
            fontSize: '11px', padding: '2px 7px', borderRadius: '10px',
            color: 'var(--color-success)',
            background: 'color-mix(in srgb, var(--color-success) 12%, transparent)',
            border: '1px solid var(--color-success)',
            cursor: loading ? 'wait' : 'pointer', opacity: loading ? 0.6 : 1,
            whiteSpace: 'nowrap',
          }}
        >
          ✓ 驗證
        </button>
      )}

      {localStatus === 'verified' && (
        <button
          disabled={loading}
          onClick={() => handleSetStatus('draft')}
          title="取消驗證，改為草稿"
          style={{
            fontSize: '11px', padding: '2px 7px', borderRadius: '10px',
            color: 'var(--color-text-muted)',
            background: 'transparent',
            border: '1px solid var(--color-border)',
            cursor: loading ? 'wait' : 'pointer', opacity: loading ? 0.6 : 0.7,
            whiteSpace: 'nowrap',
          }}
        >
          取消驗證
        </button>
      )}

      {(localStatus === 'draft' || localStatus === 'verified') && (
        <button
          disabled={loading}
          onClick={() => handleSetStatus('deprecated')}
          title="標記為已棄用"
          style={{
            fontSize: '11px', padding: '2px 7px', borderRadius: '10px',
            color: 'var(--color-text-muted)',
            background: 'transparent',
            border: '1px solid var(--color-border)',
            cursor: loading ? 'wait' : 'pointer', opacity: loading ? 0.6 : 0.5,
            whiteSpace: 'nowrap',
          }}
        >
          棄用
        </button>
      )}
    </div>
  )
}

/** TS equivalent of Rust set_frontmatter_key — patches one key in frontmatter */
function setFrontmatterKey(content: string, key: string, value: string): string {
  const prefix = `${key}:`
  const newLine = `${key}: ${value}`
  const hasFm = content.startsWith('---\n') || content.startsWith('---\r\n')
  if (hasFm) {
    const after = content.startsWith('---\r\n') ? 5 : 4
    const endOffset = content.indexOf('\n---', after)
    if (endOffset !== -1) {
      const fm = content.slice(after, endOffset)
      const rest = content.slice(endOffset)
      const lines = fm.split('\n')
      const idx = lines.findIndex(l => l.trimStart().startsWith(prefix))
      const newFm = idx >= 0
        ? lines.map((l, i) => i === idx ? newLine : l).join('\n')
        : `${newLine}\n${fm}`
      return `---\n${newFm}${rest}`
    }
  }
  return `---\n${key}: ${value}\n---\n\n${content}`
}
