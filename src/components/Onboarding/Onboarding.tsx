import { useState } from 'react'
import { open } from '@tauri-apps/plugin-dialog'
import { useSettingsStore } from '../../stores/settingsStore'

interface OnboardingProps {
  onComplete: () => void
}

export default function Onboarding({ onComplete }: OnboardingProps) {
  const [step, setStep] = useState(1)
  const [vaultPath, setVaultPath] = useState('')
  const [error, setError] = useState('')
  const { save } = useSettingsStore()

  const selectVault = async () => {
    const selected = await open({
      directory: true,
      multiple: false,
      title: '選擇或建立 Vault 資料夾',
    })
    if (selected) setVaultPath(selected as string)
  }

  const handleComplete = async () => {
    if (!vaultPath) { setError('請選擇 Vault 資料夾'); return }
    await save({ vault_path: vaultPath, onboarding_done: true })
    onComplete()
  }

  return (
    <div style={{
      width: '100%', height: '100vh',
      display: 'flex', alignItems: 'center', justifyContent: 'center',
      background: '#1a1b1e',
    }}>
      <div style={{
        width: 480, padding: '40px',
        background: '#25262b', borderRadius: '16px',
        border: '1px solid #373a40',
        boxShadow: '0 8px 24px rgba(0,0,0,0.5)',
      }}>
        {/* 步驟指示器 */}
        <div style={{ display: 'flex', gap: '8px', marginBottom: '32px' }}>
          {[1,2,3].map(i => (
            <div key={i} style={{
              flex: 1, height: '3px', borderRadius: '2px',
              background: i <= step ? '#7c8cf8' : '#373a40',
              transition: 'background 0.2s',
            }} />
          ))}
        </div>

        {step === 1 && (
          <>
            <h2 style={{ fontSize: '24px', fontWeight: 600, color: '#c9cdd4', marginBottom: '8px' }}>
              歡迎使用 noteTreeLM
            </h2>
            <p style={{ color: '#8b8fa8', marginBottom: '24px', fontSize: '14px' }}>
              選擇一個資料夾來儲存你的筆記。
            </p>
            <div
              onClick={selectVault}
              style={{
                padding: '16px', borderRadius: '8px',
                border: '1px dashed ' + (vaultPath ? '#7c8cf8' : '#373a40'),
                cursor: 'pointer', marginBottom: '16px',
                background: vaultPath ? 'rgba(124,140,248,0.05)' : 'transparent',
                transition: 'all 0.2s',
              }}
            >
              <div style={{ color: vaultPath ? '#c9cdd4' : '#8b8fa8', fontSize: '13px' }}>
                {vaultPath || '點擊選擇資料夾…'}
              </div>
            </div>
            {error && <p style={{ color: '#e06c75', fontSize: '13px', marginBottom: '12px' }}>{error}</p>}
            <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
              <button
                onClick={() => { if (!vaultPath) { setError('請選擇 Vault 資料夾'); return }; setStep(2) }}
                style={{
                  padding: '8px 20px', borderRadius: '6px',
                  background: '#7c8cf8', color: '#fff',
                  fontSize: '14px', fontWeight: 500,
                }}
              >下一步 →</button>
            </div>
          </>
        )}

        {step === 2 && (
          <>
            <h2 style={{ fontSize: '20px', fontWeight: 600, color: '#c9cdd4', marginBottom: '8px' }}>
              語音輸入設定
            </h2>
            <p style={{ color: '#8b8fa8', marginBottom: '24px', fontSize: '14px' }}>
              需要安裝 Whisper 模型才能使用語音輸入。可稍後在設定中配置。
            </p>
            <div style={{ display: 'flex', justifyContent: 'space-between' }}>
              <button
                onClick={() => setStep(1)}
                style={{ padding: '8px 16px', borderRadius: '6px', background: '#373a40', color: '#c9cdd4', fontSize: '14px' }}
              >← 上一步</button>
              <button
                onClick={() => setStep(3)}
                style={{ padding: '8px 20px', borderRadius: '6px', background: '#7c8cf8', color: '#fff', fontSize: '14px', fontWeight: 500 }}
              >下一步 →</button>
            </div>
          </>
        )}

        {step === 3 && (
          <>
            <h2 style={{ fontSize: '20px', fontWeight: 600, color: '#c9cdd4', marginBottom: '8px' }}>
              AI 功能（選填）
            </h2>
            <p style={{ color: '#8b8fa8', marginBottom: '24px', fontSize: '14px' }}>
              提供 OpenAI API Key 可啟用 LLM 主題分析和智慧摘要。可稍後在設定中配置。
            </p>
            <div style={{ display: 'flex', justifyContent: 'space-between' }}>
              <button
                onClick={() => setStep(2)}
                style={{ padding: '8px 16px', borderRadius: '6px', background: '#373a40', color: '#c9cdd4', fontSize: '14px' }}
              >← 上一步</button>
              <button
                onClick={handleComplete}
                style={{ padding: '8px 20px', borderRadius: '6px', background: '#7c8cf8', color: '#fff', fontSize: '14px', fontWeight: 500 }}
              >開始使用 ✓</button>
            </div>
          </>
        )}
      </div>
    </div>
  )
}
