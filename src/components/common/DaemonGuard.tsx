import { useState, useEffect } from 'react'

export default function DaemonGuard({ children }: { children: React.ReactNode }) {
  const [status, setStatus] = useState<'checking' | 'online' | 'offline'>('checking')

  useEffect(() => {
    const check = async () => {
      try {
        const ok = await fetch('http://127.0.0.1:7787/health').then(r => r.ok)
        setStatus(ok ? 'online' : 'offline')
      } catch {
        setStatus('offline')
      }
    }
    check()
    const interval = setInterval(check, 5000)
    return () => clearInterval(interval)
  }, [])

  if (status === 'checking') return (
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', height: '100vh', background: 'var(--color-bg-base)', flexDirection: 'column', gap: 16 }}>
      <div style={{ width: 32, height: 32, borderRadius: '50%', border: '3px solid var(--color-border)', borderTopColor: 'var(--color-accent)', animation: 'spin 1s linear infinite' }} />
      <span style={{ color: 'var(--color-text-muted)', fontSize: 14 }}>連接服務中…</span>
    </div>
  )

  if (status === 'offline') return (
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', height: '100vh', background: 'var(--color-bg-base)', flexDirection: 'column', gap: 16, padding: 32 }}>
      <div style={{ fontSize: 48 }}>⚠️</div>
      <h2 style={{ color: 'var(--color-text-primary)', margin: 0, fontSize: 18 }}>notetreelm-service 未啟動</h2>
      <p style={{ color: 'var(--color-text-muted)', fontSize: 13, textAlign: 'center', maxWidth: 400, lineHeight: 1.6, margin: 0 }}>
        請先啟動背景服務後再使用 noteTreeLM：
      </p>
      <pre style={{ background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)', borderRadius: 8, padding: '12px 20px', fontSize: 13, color: 'var(--color-text-primary)', fontFamily: 'monospace' }}>
        cargo run --manifest-path src-service/Cargo.toml
      </pre>
      <p style={{ color: 'var(--color-text-muted)', fontSize: 12, margin: 0 }}>
        或至 Settings → 背景服務 → 安裝 LaunchAgent（自動隨系統啟動）
      </p>
      <button
        onClick={() => setStatus('checking')}
        style={{ padding: '8px 20px', borderRadius: 6, background: 'var(--color-accent)', border: 'none', color: '#fff', fontSize: 13, cursor: 'pointer', marginTop: 8 }}
      >
        重新檢測
      </button>
    </div>
  )

  return <>{children}</>
}
