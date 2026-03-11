import { useEditorStore } from '../../stores/editorStore'

export type EditorAction = 'bold' | 'italic' | 'h1' | 'h2' | 'wikilink' | 'image' | 'quick_copy'

interface ToolbarProps {
  canGoBack: boolean
  canGoForward: boolean
  onBack: () => void
  onForward: () => void
  onAction: (action: EditorAction) => void
  onSave: () => void
}

export default function Toolbar({
  canGoBack, canGoForward, onBack, onForward, onAction, onSave,
}: ToolbarProps) {
  const { isDirty, viewMode, setViewMode } = useEditorStore()

  const btn = (label: string, title: string, action: () => void, active = false) => (
    <button
      key={label}
      title={title}
      onClick={action}
      style={{
        padding: '4px 8px', borderRadius: '4px', fontSize: '13px',
        color: active ? 'var(--color-accent)' : 'var(--color-text-secondary)',
        background: active ? 'var(--color-accent-dim)' : 'transparent',
        transition: 'all 0.1s',
      }}
      onMouseEnter={(e) => (e.currentTarget.style.color = 'var(--color-text-primary)')}
      onMouseLeave={(e) => (e.currentTarget.style.color = active ? 'var(--color-accent)' : 'var(--color-text-secondary)')}
    >{label}</button>
  )

  const isEditing = viewMode === 'editor' || viewMode === 'live'

  return (
    <div style={{
      display: 'flex', alignItems: 'center', gap: '4px',
      padding: '6px 12px',
      borderBottom: '1px solid var(--color-border)',
      background: 'var(--color-bg-base)',
      height: '40px',
      flexShrink: 0,
    }}>
      {/* 導覽 */}
      <button onClick={onBack} disabled={!canGoBack} title="上一篇 (⌘[)"
        style={{ color: canGoBack ? 'var(--color-text-secondary)' : 'var(--color-border)', fontSize: '16px', padding: '2px 4px' }}>‹</button>
      <button onClick={onForward} disabled={!canGoForward} title="下一篇 (⌘])"
        style={{ color: canGoForward ? 'var(--color-text-secondary)' : 'var(--color-border)', fontSize: '16px', padding: '2px 4px' }}>›</button>

      {/* 格式工具：僅在編輯模式顯示 */}
      {isEditing && <>
        <div style={{ width: '1px', height: '20px', background: 'var(--color-border)', margin: '0 4px' }} />
        {btn('B', '粗體 (⌘B)', () => onAction('bold'))}
        {btn('I', '斜體 (⌘I)', () => onAction('italic'))}
        {btn('H1', '標題 1', () => onAction('h1'))}
        {btn('H2', '標題 2', () => onAction('h2'))}
        <div style={{ width: '1px', height: '20px', background: 'var(--color-border)', margin: '0 4px' }} />
        {btn('[[', '插入 Wikilink', () => onAction('wikilink'))}
        {btn('🖼️', '插入圖片', () => onAction('image'))}
        {btn('📋', '插入快捷複製', () => onAction('quick_copy'))}
      </>}

      <div style={{ flex: 1 }} />

      {/* 未儲存指示 */}
      {isDirty && (
        <div style={{ width: '6px', height: '6px', borderRadius: '50%', background: 'var(--color-warning)' }} title="未儲存" />
      )}

      {/* 儲存（編輯模式才有意義） */}
      {isEditing && btn('儲存', '儲存 (⌘S)', onSave)}

      {/* 模式切換：主要 toggle 是「預覽 ↔ 編輯(live)」，Source 為次要 */}
      {viewMode === 'live' && <>
        {btn('👁 預覽', '切換為預覽模式', () => setViewMode('preview'))}
        {btn('Source', '切換為原始碼模式', () => setViewMode('editor'))}
      </>}
      {viewMode === 'preview' && <>
        {btn('✎ 編輯', '切換為編輯模式', () => setViewMode('live'))}
        {btn('Source', '切換為原始碼模式', () => setViewMode('editor'))}
      </>}
      {viewMode === 'editor' && <>
        {btn('✎ 編輯', '切換為即時編輯模式', () => setViewMode('live'))}
        {btn('👁 預覽', '切換為預覽模式', () => setViewMode('preview'))}
      </>}
    </div>
  )
}
