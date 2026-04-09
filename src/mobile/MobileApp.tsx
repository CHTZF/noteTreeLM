import { useState, useEffect } from 'react'
import MobilePairScreen from './MobilePairScreen'
import MobileChatPage from './MobileChatPage'
import {
  getToken,
  getBaseUrl,
  getSavedVaultId,
  saveVaultId,
  clearAuth,
  mobileApi,
} from './mobileApi'

type Screen = 'loading' | 'pair' | 'vault_select' | 'chat'

export default function MobileApp() {
  const [screen, setScreen] = useState<Screen>('loading')
  const [vaultId, setVaultId] = useState<string | null>(null)
  const [vaults, setVaults] = useState<Array<{ vault_id: string; path: string }>>([])
  const [vaultError, setVaultError] = useState('')

  useEffect(() => {
    const token = getToken()
    const base = getBaseUrl()
    if (!token || !base) {
      setScreen('pair')
      return
    }

    // Token exists — try to resolve vault
    const saved = getSavedVaultId()
    if (saved) {
      setVaultId(saved)
      setScreen('chat')
      return
    }

    // No saved vault → fetch list
    mobileApi.listVaults()
      .then(list => {
        if (list.length === 1) {
          saveVaultId(list[0].vault_id)
          setVaultId(list[0].vault_id)
          setScreen('chat')
        } else if (list.length > 1) {
          setVaults(list)
          setScreen('vault_select')
        } else {
          setVaultError('未找到任何 vault，請先在桌面端建立。')
          setScreen('vault_select')
        }
      })
      .catch(() => {
        // Token may be expired
        clearAuth()
        setScreen('pair')
      })
  }, [])

  function handlePaired() {
    // After pairing, fetch vaults
    mobileApi.listVaults()
      .then(list => {
        if (list.length === 1) {
          saveVaultId(list[0].vault_id)
          setVaultId(list[0].vault_id)
          setScreen('chat')
        } else {
          setVaults(list)
          setScreen('vault_select')
        }
      })
      .catch(() => setScreen('vault_select'))
  }

  function handleSelectVault(vid: string) {
    saveVaultId(vid)
    setVaultId(vid)
    setScreen('chat')
  }

  function handleUnpair() {
    clearAuth()
    setVaultId(null)
    setVaults([])
    setScreen('pair')
  }

  if (screen === 'loading') {
    return (
      <div style={{ minHeight: '100dvh', display: 'flex', alignItems: 'center', justifyContent: 'center', background: '#000', color: '#8e8e93' }}>
        載入中…
      </div>
    )
  }

  if (screen === 'pair') {
    return <MobilePairScreen onPaired={handlePaired} />
  }

  if (screen === 'vault_select') {
    return (
      <div style={{ minHeight: '100dvh', background: '#0f0f0f', color: '#fff', padding: '40px 20px', fontFamily: '-apple-system, BlinkMacSystemFont, sans-serif' }}>
        <div style={{ fontSize: 22, fontWeight: 700, marginBottom: 8 }}>選擇 Vault</div>
        {vaultError && <div style={{ color: '#ff453a', marginBottom: 16, fontSize: 14 }}>{vaultError}</div>}
        {vaults.map(v => (
          <div
            key={v.vault_id}
            style={{ background: '#1c1c1e', borderRadius: 12, padding: '16px', marginBottom: 10, cursor: 'pointer' }}
            onClick={() => handleSelectVault(v.vault_id)}
          >
            <div style={{ fontWeight: 600 }}>{v.path.split('/').pop() ?? v.vault_id}</div>
            <div style={{ color: '#8e8e93', fontSize: 12, marginTop: 4 }}>{v.path}</div>
          </div>
        ))}
      </div>
    )
  }

  return <MobileChatPage vaultId={vaultId!} onUnpair={handleUnpair} />
}
