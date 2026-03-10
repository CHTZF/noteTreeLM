import { useState } from 'react'
import { open as openDialog } from '@tauri-apps/plugin-dialog'
import { useSettingsStore } from '../../stores/settingsStore'

interface Props {
  onSelect: (path: string) => void
  canClose?: boolean
  onClose?: () => void
}

export default function VaultManagerModal({ onSelect, canClose, onClose }: Props) {
  const { settings } = useSettingsStore()
  const [search, setSearch] = useState('')

  const recentVaults: string[] = settings.recent_vaults ?? []
  const currentVault = settings.vault_path
  const allVaults = [
    ...recentVaults,
    ...(currentVault && !recentVaults.includes(currentVault) ? [currentVault] : []),
  ]
  const filteredVaults = search.trim()
    ? allVaults.filter(v => {
        const name = v.split('/').pop() || v
        return name.toLowerCase().includes(search.trim().toLowerCase()) ||
          v.toLowerCase().includes(search.trim().toLowerCase())
      })
    : allVaults

  const handleSelectExisting = async () => {
    const result = await openDialog({ directory: true, multiple: false, title: '選擇工作區資料夾' })
    if (result) onSelect(result as string)
  }

  const handleCreate = async () => {
    const result = await openDialog({ directory: true, multiple: false, title: '選擇新工作區位置' })
    if (result) onSelect(result as string)
  }

  return (
    <div style={{
      position: 'fixed', inset: 0, top: 'var(--titlebar-height)',
      background: 'rgba(0,0,0,0.65)',
      display: 'flex', alignItems: 'center', justifyContent: 'center',
      zIndex: 1200,
    }}>
      <div style={{
        width: 680, maxHeight: '70vh',
        background: 'var(--color-bg-elevated)',
        border: '1px solid var(--color-border)',
        borderRadius: '14px',
        display: 'flex', flexDirection: 'column',
        boxShadow: '0 16px 60px rgba(0,0,0,0.5)',
        overflow: 'hidden',
      }}>
        {/* Header */}
        <div style={{
          padding: '18px 24px',
          borderBottom: '1px solid var(--color-border)',
          display: 'flex', alignItems: 'center', justifyContent: 'space-between',
          flexShrink: 0,
        }}>
          <div style={{ fontSize: '16px', fontWeight: 600, color: 'var(--color-text-primary)' }}>
            工作區管理
          </div>
          {canClose && onClose && (
            <button
              onClick={onClose}
              style={{
                background: 'none', border: 'none', cursor: 'pointer',
                color: 'var(--color-text-muted)', fontSize: '20px',
                lineHeight: 1, padding: '2px 6px', borderRadius: '4px',
              }}
            >×</button>
          )}
        </div>

        {/* Body */}
        <div style={{ display: 'flex', flex: 1, minHeight: 0 }}>
          {/* Left: vault list */}
          <div style={{
            flex: 1, padding: '16px 12px',
            borderRight: '1px solid var(--color-border)',
            overflowY: 'auto',
            display: 'flex', flexDirection: 'column', gap: '4px',
          }}>
            <div style={{
              fontSize: '11px', fontWeight: 600, color: 'var(--color-text-muted)',
              textTransform: 'uppercase', letterSpacing: '0.5px',
              padding: '0 4px', marginBottom: '8px',
            }}>
              已有工作區
            </div>
            {allVaults.length > 0 && (
              <input
                type="text"
                placeholder="搜尋工作區名稱…"
                value={search}
                onChange={e => setSearch(e.target.value)}
                style={{
                  width: '100%', padding: '6px 10px', boxSizing: 'border-box',
                  background: 'var(--color-bg-base)', border: '1px solid var(--color-border)',
                  borderRadius: '6px', color: 'var(--color-text-primary)', fontSize: '13px',
                  outline: 'none', marginBottom: '8px',
                }}
              />
            )}
            {allVaults.length === 0 ? (
              <div style={{
                color: 'var(--color-text-muted)', fontSize: '13px',
                padding: '16px 4px',
              }}>
                尚無工作區記錄，請從右側新建或選擇資料夾。
              </div>
            ) : filteredVaults.length === 0 ? (
              <div style={{ color: 'var(--color-text-muted)', fontSize: '13px', padding: '12px 4px' }}>
                無符合結果
              </div>
            ) : filteredVaults.map(v => {
              const name = v.split('/').pop() || v
              const isCurrent = v === currentVault
              return (
                <button
                  key={v}
                  onClick={() => onSelect(v)}
                  style={{
                    display: 'flex', flexDirection: 'column', gap: '2px', textAlign: 'left',
                    padding: '10px 12px', borderRadius: '8px',
                    background: isCurrent ? 'rgba(var(--color-accent-rgb, 124,140,248),0.12)' : 'transparent',
                    border: '1px solid ' + (isCurrent ? 'var(--color-accent)' : 'transparent'),
                    cursor: 'pointer',
                    transition: 'background 0.12s',
                  }}
                  onMouseEnter={e => {
                    if (!isCurrent) (e.currentTarget as HTMLElement).style.background = 'var(--color-bg-hover, rgba(255,255,255,0.06))'
                  }}
                  onMouseLeave={e => {
                    if (!isCurrent) (e.currentTarget as HTMLElement).style.background = 'transparent'
                  }}
                >
                  <span style={{ fontSize: '13px', fontWeight: 500, color: 'var(--color-text-primary)' }}>
                    {name}
                    {isCurrent && (
                      <span style={{ marginLeft: '6px', fontSize: '10px', color: 'var(--color-accent)', fontWeight: 400 }}>
                        目前
                      </span>
                    )}
                  </span>
                  <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>{v}</span>
                </button>
              )
            })}
          </div>

          {/* Right: action buttons */}
          <div style={{
            width: 200, padding: '16px',
            display: 'flex', flexDirection: 'column', gap: '10px',
            flexShrink: 0,
          }}>
            <div style={{
              fontSize: '11px', fontWeight: 600, color: 'var(--color-text-muted)',
              textTransform: 'uppercase', letterSpacing: '0.5px', marginBottom: '2px',
            }}>
              動作
            </div>
            <button
              onClick={handleCreate}
              style={{
                padding: '10px 16px', borderRadius: '8px',
                background: 'var(--color-accent)', color: '#fff',
                border: 'none', cursor: 'pointer',
                fontSize: '13px', fontWeight: 500, textAlign: 'left',
              }}
            >
              + 創建新工作區
            </button>
            <button
              onClick={handleSelectExisting}
              style={{
                padding: '10px 16px', borderRadius: '8px',
                background: 'var(--color-bg-base)', color: 'var(--color-text-primary)',
                border: '1px solid var(--color-border)', cursor: 'pointer',
                fontSize: '13px', fontWeight: 500, textAlign: 'left',
              }}
            >
              選擇已有資料夾
            </button>
          </div>
        </div>
      </div>
    </div>
  )
}
