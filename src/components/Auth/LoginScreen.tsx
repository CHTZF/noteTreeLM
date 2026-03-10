import { useState, useEffect } from 'react'
import { useAuthStore } from '../../stores/authStore'

export default function LoginScreen() {
  const { login, isLoading, error, clearError } = useAuthStore()
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [submitting, setSubmitting] = useState(false)

  useEffect(() => {
    if (error) clearError()
  }, [username, password])

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!username.trim() || !password) return
    setSubmitting(true)
    try {
      await login(username.trim(), password)
    } catch {
      // error shown via store
    } finally {
      setSubmitting(false)
    }
  }

  const inputStyle: React.CSSProperties = {
    width: '100%',
    padding: '10px 14px',
    borderRadius: '8px',
    border: '1px solid var(--color-border)',
    background: 'var(--color-bg-base)',
    color: 'var(--color-text-primary)',
    fontSize: '14px',
    outline: 'none',
    boxSizing: 'border-box',
  }

  return (
    <div style={{
      flex: 1,
      background: 'var(--color-bg-base)',
      display: 'flex', alignItems: 'center', justifyContent: 'center',
    }}>
      <div style={{
        width: 360,
        background: 'var(--color-bg-elevated)',
        border: '1px solid var(--color-border)',
        borderRadius: '14px',
        padding: '36px 32px',
        display: 'flex', flexDirection: 'column', gap: '20px',
        boxShadow: '0 8px 40px rgba(0,0,0,0.3)',
      }}>
        {/* Logo / Title */}
        <div style={{ textAlign: 'center' }}>
          <div style={{ fontSize: '28px', marginBottom: '6px' }}>📝</div>
          <div style={{ fontSize: '20px', fontWeight: 700, color: 'var(--color-text-primary)' }}>
            noteTreeLM
          </div>
          <div style={{ fontSize: '13px', color: 'var(--color-text-muted)', marginTop: '4px' }}>
            請登入以繼續
          </div>
        </div>

        <form onSubmit={handleSubmit} style={{ display: 'flex', flexDirection: 'column', gap: '12px' }}>
          <div style={{ display: 'flex', flexDirection: 'column', gap: '6px' }}>
            <label style={{ fontSize: '12px', color: 'var(--color-text-secondary)', fontWeight: 500 }}>
              帳號
            </label>
            <input
              type="text"
              value={username}
              onChange={e => setUsername(e.target.value)}
              placeholder="請輸入帳號"
              autoFocus
              autoComplete="username"
              style={inputStyle}
            />
          </div>

          <div style={{ display: 'flex', flexDirection: 'column', gap: '6px' }}>
            <label style={{ fontSize: '12px', color: 'var(--color-text-secondary)', fontWeight: 500 }}>
              密碼
            </label>
            <input
              type="password"
              value={password}
              onChange={e => setPassword(e.target.value)}
              placeholder="••••••"
              autoComplete="current-password"
              style={inputStyle}
            />
          </div>

          {error && (
            <div style={{
              padding: '8px 12px',
              borderRadius: '6px',
              background: 'rgba(224,64,64,0.12)',
              border: '1px solid rgba(224,64,64,0.3)',
              color: 'var(--color-error, #e04040)',
              fontSize: '13px',
            }}>
              {error}
            </div>
          )}

          <button
            type="submit"
            disabled={submitting || isLoading || !username.trim() || !password}
            style={{
              marginTop: '4px',
              padding: '10px',
              borderRadius: '8px',
              border: 'none',
              background: 'var(--color-accent)',
              color: '#fff',
              fontSize: '14px',
              fontWeight: 600,
              cursor: submitting ? 'wait' : 'pointer',
              opacity: (submitting || !username.trim() || !password) ? 0.6 : 1,
              transition: 'opacity 0.15s',
            }}
          >
            {submitting ? '登入中…' : '登入'}
          </button>
        </form>
      </div>
    </div>
  )
}
