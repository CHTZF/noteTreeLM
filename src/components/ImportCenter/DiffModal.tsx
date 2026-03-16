interface PageUpdateInfo {
  page_id: string
  url: string
  title: string
  note_path: string
  new_content: string
}

interface Props {
  update: PageUpdateInfo
  onApply: (pageId: string, newContent: string) => void
  onSkip: (pageId: string) => void
  onClose: () => void
}

export default function DiffModal({ update, onApply, onSkip, onClose }: Props) {
  return (
    <div style={{
      position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.5)',
      display: 'flex', alignItems: 'center', justifyContent: 'center',
      zIndex: 500,
    }} onClick={onClose}>
      <div
        onClick={e => e.stopPropagation()}
        style={{
          background: 'var(--color-bg-elevated)',
          border: '1px solid var(--color-border)',
          borderRadius: 10,
          width: '70vw', maxWidth: 900,
          maxHeight: '80vh',
          display: 'flex', flexDirection: 'column',
          boxShadow: '0 8px 40px rgba(0,0,0,0.4)',
        }}
      >
        {/* Header */}
        <div style={{
          padding: '14px 18px 12px',
          borderBottom: '1px solid var(--color-border)',
          display: 'flex', alignItems: 'center', gap: 10,
        }}>
          <span style={{ fontWeight: 600, fontSize: 14, flex: 1, color: 'var(--color-text-primary)' }}>
            頁面更新：{update.title}
          </span>
          <a href={update.url} target="_blank" rel="noreferrer"
            style={{ fontSize: 11, color: 'var(--color-accent)', textDecoration: 'none' }}>
            {update.url}
          </a>
          <button onClick={onClose} style={{
            background: 'none', border: 'none', cursor: 'pointer',
            fontSize: 16, color: 'var(--color-text-muted)', padding: '2px 6px',
          }}>✕</button>
        </div>

        {/* Content preview */}
        <div style={{ flex: 1, overflow: 'auto', padding: '12px 18px' }}>
          <div style={{ marginBottom: 8, fontSize: 11, color: 'var(--color-text-muted)' }}>
            新內容預覽（將取代 {update.note_path}）
          </div>
          <pre style={{
            background: 'var(--color-bg)',
            border: '1px solid var(--color-border)',
            borderRadius: 6,
            padding: '10px 12px',
            fontSize: 12,
            fontFamily: 'var(--font-mono, monospace)',
            overflow: 'auto',
            maxHeight: '50vh',
            whiteSpace: 'pre-wrap',
            wordBreak: 'break-word',
            color: 'var(--color-text-primary)',
            margin: 0,
          }}>
            {update.new_content.slice(0, 8000)}{update.new_content.length > 8000 ? '\n…（已截斷）' : ''}
          </pre>
        </div>

        {/* Actions */}
        <div style={{
          padding: '12px 18px',
          borderTop: '1px solid var(--color-border)',
          display: 'flex', justifyContent: 'flex-end', gap: 8,
        }}>
          <button
            onClick={() => onSkip(update.page_id)}
            style={{
              padding: '6px 16px', borderRadius: 6, fontSize: 13,
              background: 'none', border: '1px solid var(--color-border)',
              cursor: 'pointer', color: 'var(--color-text-primary)',
            }}
          >
            跳過
          </button>
          <button
            onClick={() => onApply(update.page_id, update.new_content)}
            style={{
              padding: '6px 16px', borderRadius: 6, fontSize: 13,
              background: 'var(--color-accent)', border: 'none',
              cursor: 'pointer', color: '#fff', fontWeight: 600,
            }}
          >
            套用更新
          </button>
        </div>
      </div>
    </div>
  )
}
