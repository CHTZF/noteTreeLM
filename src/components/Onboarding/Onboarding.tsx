import { useState } from 'react'
import { open } from '@tauri-apps/plugin-dialog'
import { useSettingsStore } from '../../stores/settingsStore'
import ModelDownloader, { EMBEDDING_MODELS } from '../Settings/ModelDownloader'

interface OnboardingProps {
  onComplete: () => void
}

export default function Onboarding({ onComplete }: OnboardingProps) {
  const [step, setStep] = useState(1)
  const [vaultPath, setVaultPath] = useState('')
  const [embeddingModelPath, setEmbeddingModelPath] = useState('')
  const [error, setError] = useState('')
  const { saveSystem, savePersonal } = useSettingsStore()

  const TOTAL_STEPS = 4

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
    const systemPatch: Record<string, unknown> = { system_current_vault_path: vaultPath }
    if (embeddingModelPath) systemPatch.embedding_model_path = embeddingModelPath
    await saveSystem(systemPatch as any)
    await savePersonal({ onboarding_done: true, personal_current_vault_path: vaultPath })
    onComplete()
  }

  const btnBase: React.CSSProperties = {
    padding: '8px 20px', borderRadius: '6px',
    fontSize: '14px', fontWeight: 500, cursor: 'pointer',
  }
  const btnPrimary: React.CSSProperties = { ...btnBase, background: '#7c8cf8', color: '#fff' }
  const btnSecondary: React.CSSProperties = { ...btnBase, background: '#373a40', color: '#c9cdd4' }

  return (
    <div style={{
      width: '100%', height: '100vh',
      display: 'flex', alignItems: 'center', justifyContent: 'center',
      background: '#1a1b1e',
    }}>
      <div style={{
        width: 520, padding: '40px',
        background: '#25262b', borderRadius: '16px',
        border: '1px solid #373a40',
        boxShadow: '0 8px 24px rgba(0,0,0,0.5)',
        maxHeight: '90vh', overflowY: 'auto',
      }}>
        {/* 步驟指示器 */}
        <div style={{ display: 'flex', gap: '8px', marginBottom: '32px' }}>
          {Array.from({ length: TOTAL_STEPS }, (_, i) => i + 1).map(i => (
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
                onClick={() => { if (!vaultPath) { setError('請選擇 Vault 資料夾'); return }; setError(''); setStep(2) }}
                style={btnPrimary}
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
              需要安裝 Whisper 模型才能使用語音輸入。可稍後在系統設定 → 本機模型 中配置。
            </p>
            <div style={{ display: 'flex', justifyContent: 'space-between' }}>
              <button onClick={() => setStep(1)} style={btnSecondary}>← 上一步</button>
              <button onClick={() => setStep(3)} style={btnPrimary}>下一步 →</button>
            </div>
          </>
        )}

        {step === 3 && (
          <>
            <h2 style={{ fontSize: '20px', fontWeight: 600, color: '#c9cdd4', marginBottom: '8px' }}>
              語意搜尋（Embedding）
            </h2>
            <p style={{ color: '#8b8fa8', marginBottom: '20px', fontSize: '14px' }}>
              Embedding 模型讓 AI 能夠理解筆記語義，提供更準確的搜尋結果。可選擇跳過，稍後在系統設定中配置。
            </p>
            <ModelDownloader
              models={EMBEDDING_MODELS}
              title="Embedding 模型"
              kind="llm"
              value={embeddingModelPath}
              onChange={setEmbeddingModelPath}
            />
            <div style={{ display: 'flex', justifyContent: 'space-between', marginTop: '24px' }}>
              <button onClick={() => setStep(2)} style={btnSecondary}>← 上一步</button>
              <button onClick={() => setStep(4)} style={btnPrimary}>下一步 →</button>
            </div>
          </>
        )}

        {step === 4 && (
          <>
            <h2 style={{ fontSize: '20px', fontWeight: 600, color: '#c9cdd4', marginBottom: '8px' }}>
              AI 功能（選填）
            </h2>
            <p style={{ color: '#8b8fa8', marginBottom: '24px', fontSize: '14px' }}>
              提供 OpenAI API Key 可啟用 LLM 主題分析和智慧摘要。可稍後在設定中配置。
            </p>
            <div style={{ display: 'flex', justifyContent: 'space-between' }}>
              <button onClick={() => setStep(3)} style={btnSecondary}>← 上一步</button>
              <button onClick={handleComplete} style={btnPrimary}>開始使用 ✓</button>
            </div>
          </>
        )}
      </div>
    </div>
  )
}
