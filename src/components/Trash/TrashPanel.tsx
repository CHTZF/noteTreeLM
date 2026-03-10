import { useState, useEffect, useCallback } from 'react'
import { useVaultStore } from '../../stores/vaultStore'
import { TrashItem, FileTreeNode } from '../../types/models'
import { toast } from '../common/Toast'
import { ask } from '@tauri-apps/plugin-dialog'

interface TrashPanelProps {
  onClose?: () => void
  inline?: boolean
}

// 遞迴收集所有資料夾路徑
function collectFolderPaths(nodes: FileTreeNode[], result: string[] = []): string[] {
  for (const node of nodes) {
    if (node.isFolder) {
      result.push(node.path)
      if (node.children) collectFolderPaths(node.children, result)
    }
  }
  return result
}

function formatTime(ts: number): string {
  const d = new Date(ts)
  return (
    `${d.getFullYear()}/${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')} ` +
    `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
  )
}

export default function TrashPanel({ onClose, inline }: TrashPanelProps) {
  const { fileTree, listTrash, restoreTrashItem, deleteTrashItems } = useVaultStore()
  const [items, setItems] = useState<TrashItem[]>([])
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [loading, setLoading] = useState(true)
  const [showFolderPicker, setShowFolderPicker] = useState(false)
  const [pendingRestoreIds, setPendingRestoreIds] = useState<string[]>([])

  const loadItems = useCallback(async () => {
    try {
      const result = await listTrash()
      setItems(result)
    } catch (e: any) {
      toast.error(e.message || '載入垃圾桶失敗')
    } finally {
      setLoading(false)
    }
  }, [listTrash])

  useEffect(() => { loadItems() }, [loadItems])

  const allSelected = items.length > 0 && selected.size === items.length
  const toggleAll = () => {
    setSelected(allSelected ? new Set() : new Set(items.map(i => i.id)))
  }
  const toggleItem = (id: string) => {
    const next = new Set(selected)
    if (next.has(id)) { next.delete(id) } else { next.add(id) }
    setSelected(next)
  }

  const handleRestoreClick = () => {
    if (selected.size === 0) return
    setPendingRestoreIds(Array.from(selected))
    setShowFolderPicker(true)
  }

  const handleFolderSelect = async (targetFolder: string) => {
    setShowFolderPicker(false)
    let successCount = 0
    for (const id of pendingRestoreIds) {
      try {
        await restoreTrashItem(id, targetFolder)
        successCount++
      } catch (e: any) {
        toast.error(e.message || '復原失敗')
      }
    }
    if (successCount > 0) {
      setSelected(new Set())
      await loadItems()
      toast.success(`已復原 ${successCount} 個筆記，並置頂於「${targetFolder || '根目錄'}」`)
    }
    setPendingRestoreIds([])
  }

  const handlePermanentDelete = async () => {
    if (selected.size === 0) return
    const confirmed = await ask(
      `確定要徹底刪除選取的 ${selected.size} 個筆記嗎？此操作無法復原。`,
      { title: '徹底刪除', kind: 'warning' }
    )
    if (!confirmed) return
    try {
      await deleteTrashItems(Array.from(selected))
      setSelected(new Set())
      await loadItems()
      toast.success('已徹底刪除')
    } catch (e: any) {
      toast.error(e.message || '刪除失敗')
    }
  }

  const folderPaths = ['', ...collectFolderPaths(fileTree)]

  const folderPickerOverlay = showFolderPicker && (
    <div
      onClick={(e) => { if (e.target === e.currentTarget) setShowFolderPicker(false) }}
      style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.45)', zIndex: 600, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
      <div style={{ background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '10px', width: '320px', maxHeight: '52vh', display: 'flex', flexDirection: 'column', boxShadow: '0 20px 60px rgba(0,0,0,0.5)' }}>
        <div style={{ padding: '14px 20px', borderBottom: '1px solid var(--color-border)', fontSize: '14px', fontWeight: 600, color: 'var(--color-text-primary)', flexShrink: 0 }}>
          選擇復原位置
        </div>
        <div style={{ flex: 1, overflowY: 'auto', padding: '4px 0', minHeight: 0 }}>
          {folderPaths.map((fp) => (
            <button
              key={fp || '__root__'}
              onClick={() => handleFolderSelect(fp)}
              style={{ display: 'flex', alignItems: 'center', gap: '8px', width: '100%', padding: '9px 20px', fontSize: '13px', textAlign: 'left', color: 'var(--color-text-secondary)', background: 'transparent', cursor: 'pointer' }}
              onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
              onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}>
              <span style={{ fontSize: '14px' }}>{fp ? '📁' : '📂'}</span>
              <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                {fp || '根目錄'}
              </span>
            </button>
          ))}
        </div>
        <div style={{ padding: '10px 20px', borderTop: '1px solid var(--color-border)', flexShrink: 0 }}>
          <button
            onClick={() => setShowFolderPicker(false)}
            style={{ width: '100%', padding: '7px', borderRadius: '6px', fontSize: '13px', color: 'var(--color-text-muted)', background: 'var(--color-bg-hover)', cursor: 'pointer' }}>
            取消
          </button>
        </div>
      </div>
    </div>
  )

  const cardContent = (
    <div style={inline
      ? { display: 'flex', flexDirection: 'column', flex: 1, overflow: 'hidden', background: 'var(--color-bg-surface)' }
      : { background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: '10px', width: '480px', maxHeight: '72vh', display: 'flex', flexDirection: 'column', boxShadow: '0 20px 60px rgba(0,0,0,0.5)' }
    }>
      {/* Header */}
      <div style={{ display: 'flex', alignItems: 'center', padding: '16px 20px', borderBottom: '1px solid var(--color-border)', flexShrink: 0 }}>
        <span style={{ fontSize: '18px', marginRight: '8px' }}>🗑</span>
        <span style={{ fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)', flex: 1 }}>垃圾桶</span>
        {!inline && onClose && (
          <button
            onClick={onClose}
            style={{ color: 'var(--color-text-muted)', fontSize: '16px', padding: '2px 6px', borderRadius: '4px' }}
            onMouseEnter={(e) => (e.currentTarget.style.color = 'var(--color-text-primary)')}
            onMouseLeave={(e) => (e.currentTarget.style.color = 'var(--color-text-muted)')}>✕</button>
        )}
      </div>

      {/* Select all bar */}
      {items.length > 0 && (
        <div style={{ display: 'flex', alignItems: 'center', padding: '8px 20px', borderBottom: '1px solid var(--color-border)', background: 'var(--color-bg-base)', flexShrink: 0 }}>
          <label style={{ display: 'flex', alignItems: 'center', gap: '8px', cursor: 'pointer', fontSize: '13px', color: 'var(--color-text-secondary)', userSelect: 'none' }}>
            <input
              type="checkbox"
              checked={allSelected}
              onChange={toggleAll}
              style={{ width: '14px', height: '14px', cursor: 'pointer' }} />
            全選（共 {items.length} 個）
          </label>
        </div>
      )}

      {/* Item list */}
      <div style={{ flex: 1, overflowY: 'auto', minHeight: 0 }}>
        {loading ? (
          <div style={{ padding: '48px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: '13px' }}>載入中…</div>
        ) : items.length === 0 ? (
          <div style={{ padding: '48px', textAlign: 'center', color: 'var(--color-text-muted)', fontSize: '13px' }}>
            <div style={{ fontSize: '36px', marginBottom: '12px', opacity: 0.4 }}>🗑</div>
            垃圾桶是空的
          </div>
        ) : (
          items.map((item) => {
            const isSelected = selected.has(item.id)
            return (
              <label
                key={item.id}
                style={{ display: 'flex', alignItems: 'flex-start', gap: '10px', padding: '10px 20px', cursor: 'pointer', background: isSelected ? 'var(--color-accent-dim)' : 'transparent', borderBottom: '1px solid var(--color-border)', userSelect: 'none', transition: 'background 0.08s' }}
                onMouseEnter={(e) => { if (!isSelected) (e.currentTarget as HTMLElement).style.background = 'var(--color-bg-hover)' }}
                onMouseLeave={(e) => { if (!isSelected) (e.currentTarget as HTMLElement).style.background = 'transparent' }}>
                <input
                  type="checkbox"
                  checked={isSelected}
                  onChange={() => toggleItem(item.id)}
                  onClick={(e) => e.stopPropagation()}
                  style={{ width: '14px', height: '14px', flexShrink: 0, marginTop: '3px', cursor: 'pointer' }} />
                <span style={{ fontSize: '14px', flexShrink: 0, marginTop: '1px' }}>📝</span>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ fontSize: '13px', fontWeight: 500, color: 'var(--color-text-primary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {item.title || item.name}
                  </div>
                  <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '2px', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    原始位置：{item.original_path}
                  </div>
                  <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '1px' }}>
                    {formatTime(item.deleted_at)}
                  </div>
                </div>
              </label>
            )
          })
        )}
      </div>

      {/* Footer */}
      {items.length > 0 && (
        <div style={{ display: 'flex', gap: '8px', padding: '12px 20px', borderTop: '1px solid var(--color-border)', flexShrink: 0 }}>
          <button
            onClick={handleRestoreClick}
            disabled={selected.size === 0}
            style={{
              flex: 1, padding: '8px 12px', borderRadius: '6px', fontSize: '13px', fontWeight: 500,
              background: selected.size > 0 ? 'var(--color-accent)' : 'var(--color-bg-hover)',
              color: selected.size > 0 ? '#fff' : 'var(--color-text-muted)',
              cursor: selected.size > 0 ? 'pointer' : 'not-allowed', transition: 'opacity 0.1s',
            }}>
            復原{selected.size > 0 ? ` (${selected.size})` : ''}
          </button>
          <button
            onClick={handlePermanentDelete}
            disabled={selected.size === 0}
            style={{
              flex: 1, padding: '8px 12px', borderRadius: '6px', fontSize: '13px', fontWeight: 500,
              background: selected.size > 0 ? 'rgba(224,108,117,0.12)' : 'var(--color-bg-hover)',
              color: selected.size > 0 ? 'var(--color-danger, #e06c75)' : 'var(--color-text-muted)',
              border: selected.size > 0 ? '1px solid rgba(224,108,117,0.35)' : '1px solid transparent',
              cursor: selected.size > 0 ? 'pointer' : 'not-allowed', transition: 'opacity 0.1s',
            }}>
            徹底刪除{selected.size > 0 ? ` (${selected.size})` : ''}
          </button>
        </div>
      )}
    </div>
  )

  if (inline) {
    return (
      <>
        {cardContent}
        {folderPickerOverlay}
      </>
    )
  }

  return (
    <div
      onClick={(e) => { if (e.target === e.currentTarget) onClose?.() }}
      style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.6)', zIndex: 500, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
      {cardContent}
      {folderPickerOverlay}
    </div>
  )
}
