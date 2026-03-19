import { SkillsHub } from '../ImportCenter/ImportPanel'

export default function SkillsPage() {
  return (
    <div style={{
      display: 'flex',
      flexDirection: 'column',
      height: '100%',
      overflow: 'hidden',
      background: 'var(--color-bg-base)',
    }}>
      <div style={{
        padding: '10px 14px 6px',
        borderBottom: '1px solid var(--color-border)',
        flexShrink: 0,
        display: 'flex',
        alignItems: 'center',
        gap: 8,
      }}>
        <span style={{ fontSize: 13, fontWeight: 600, color: 'var(--color-text-secondary)', letterSpacing: '0.05em' }}>
          技能規範
        </span>
      </div>
      <div style={{ flex: 1, overflowY: 'auto' }}>
        <SkillsHub />
      </div>
    </div>
  )
}
