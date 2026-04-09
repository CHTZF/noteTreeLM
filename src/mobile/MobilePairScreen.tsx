import { useState } from 'react'
import { mobileApi, saveAuth } from './mobileApi'

interface Props {
  onPaired: () => void
}

export default function MobilePairScreen({ onPaired }: Props) {
  const [url, setUrl] = useState('')
  const [code, setCode] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')

  async function handlePair() {
    setError('')
    if (!url.trim() || !code.trim()) {
      setError('請填入 Tunnel URL 和配對碼')
      return
    }
    setLoading(true)
    try {
      const resp = await mobileApi.mobilePair(url.trim(), code.trim(), 'Mobile')
      saveAuth(url.trim(), resp.token, resp.username)
      onPaired()
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : '配對失敗')
    } finally {
      setLoading(false)
    }
  }

  return (
    <div style={styles.container}>
      <div style={styles.card}>
        <div style={styles.title}>連接你的 noteTreeLM</div>
        <div style={styles.subtitle}>
          在桌面端「設定 → Mobile」掃描 QR code 或手動輸入資訊
        </div>

        <label style={styles.label}>Tunnel URL</label>
        <input
          style={styles.input}
          type="url"
          placeholder="https://xxxx.trycloudflare.com"
          value={url}
          onChange={e => setUrl(e.target.value)}
          autoCapitalize="none"
          autoCorrect="off"
        />

        <label style={styles.label}>配對碼</label>
        <input
          style={styles.input}
          type="text"
          placeholder="e.g. a3f2b1..."
          value={code}
          onChange={e => setCode(e.target.value)}
          autoCapitalize="none"
          autoCorrect="off"
        />

        {error && <div style={styles.error}>{error}</div>}

        <button
          style={{ ...styles.button, opacity: loading ? 0.6 : 1 }}
          onClick={handlePair}
          disabled={loading}
        >
          {loading ? '配對中…' : '配對'}
        </button>
      </div>
    </div>
  )
}

const styles: Record<string, React.CSSProperties> = {
  container: {
    minHeight: '100dvh',
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'center',
    background: '#0f0f0f',
    padding: '20px',
  },
  card: {
    width: '100%',
    maxWidth: 400,
    background: '#1c1c1e',
    borderRadius: 16,
    padding: '28px 24px',
    boxShadow: '0 4px 32px rgba(0,0,0,0.5)',
  },
  title: {
    fontSize: 22,
    fontWeight: 700,
    color: '#fff',
    marginBottom: 8,
  },
  subtitle: {
    fontSize: 13,
    color: '#8e8e93',
    marginBottom: 24,
    lineHeight: 1.5,
  },
  label: {
    display: 'block',
    fontSize: 12,
    color: '#8e8e93',
    marginBottom: 6,
    marginTop: 16,
  },
  input: {
    width: '100%',
    background: '#2c2c2e',
    border: '1px solid #3a3a3c',
    borderRadius: 10,
    padding: '12px 14px',
    color: '#fff',
    fontSize: 15,
    outline: 'none',
    boxSizing: 'border-box',
  },
  error: {
    marginTop: 12,
    color: '#ff453a',
    fontSize: 13,
  },
  button: {
    marginTop: 24,
    width: '100%',
    background: '#0a84ff',
    color: '#fff',
    border: 'none',
    borderRadius: 12,
    padding: '14px',
    fontSize: 16,
    fontWeight: 600,
    cursor: 'pointer',
  },
}
