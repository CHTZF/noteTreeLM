import { useState, useEffect } from 'react'
import { useAuthStore } from '../../stores/authStore'

export default function LoginScreen() {
  const { login, loginWithGoogle, isLoading, error, clearError } = useAuthStore()
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [submitting, setSubmitting] = useState(false)
  const [googleLoading, setGoogleLoading] = useState(false)

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

  const handleGoogleLogin = async () => {
    setGoogleLoading(true)
    try {
      await loginWithGoogle()
    } catch {
      // error shown via store
    } finally {
      setGoogleLoading(false)
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

        {/* 分隔線 */}
        <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
          <div style={{ flex: 1, height: '1px', background: 'var(--color-border)' }} />
          <span style={{ fontSize: '12px', color: 'var(--color-text-muted)' }}>或</span>
          <div style={{ flex: 1, height: '1px', background: 'var(--color-border)' }} />
        </div>

        {/* Google 登入按鈕 */}
        <button
          onClick={handleGoogleLogin}
          disabled={googleLoading || submitting || isLoading}
          style={{
            display: 'flex', alignItems: 'center', justifyContent: 'center', gap: '10px',
            padding: '10px',
            borderRadius: '8px',
            border: '1px solid var(--color-border)',
            background: 'var(--color-bg-base)',
            color: 'var(--color-text-primary)',
            fontSize: '14px',
            fontWeight: 500,
            cursor: googleLoading ? 'wait' : 'pointer',
            opacity: (googleLoading || submitting) ? 0.6 : 1,
            transition: 'opacity 0.15s',
          }}
        >
          {/* Google SVG logo */}
          {!googleLoading && (
            <svg width="18" height="18" viewBox="0 0 48 48">
              <path fill="#EA4335" d="M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5z"/>
              <path fill="#4285F4" d="M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.78 7.18l7.73 6c4.51-4.18 7.09-10.36 7.09-17.65z"/>
              <path fill="#FBBC05" d="M10.53 28.59c-.48-1.45-.76-2.99-.76-4.59s.27-3.14.76-4.59l-7.98-6.19C.92 16.46 0 20.12 0 24c0 3.88.92 7.54 2.56 10.78l7.97-6.19z"/>
              <path fill="#34A853" d="M24 48c6.48 0 11.93-2.13 15.89-5.81l-7.73-6c-2.18 1.48-4.97 2.31-8.16 2.31-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48z"/>
              <path fill="none" d="M0 0h48v48H0z"/>
            </svg>
          )}
          {googleLoading ? '開啟瀏覽器中…' : '使用 Google 帳號登入'}
        </button>
      </div>
    </div>
  )
}
