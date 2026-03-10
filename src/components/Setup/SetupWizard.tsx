import { useState, useCallback } from 'react'
import { open } from '@tauri-apps/plugin-dialog'
import { useSettingsStore } from '../../stores/settingsStore'
import ModelDownloader, { WHISPER_MODELS, LLM_MODELS } from '../Settings/ModelDownloader'

interface Props {
  onDone: () => void
}

export default function SetupWizard({ onDone }: Props) {
  const { settings, save: saveSettings } = useSettingsStore()
  const [whisperCli, setWhisperCli] = useState(settings.whisper_cli_path)
  const [whisperModel, setWhisperModel] = useState(settings.whisper_model_path)
  const [llamaCli, setLlamaCli] = useState(settings.llama_cli_path)
  const [llamaModel, setLlamaModel] = useState(settings.llm_model_path)
  const [saving, setSaving] = useState(false)

  const allSet = !!whisperCli && !!whisperModel && !!llamaCli && !!llamaModel

  const pickFile = useCallback(async (onChange: (v: string) => void, current: string) => {
    const defaultPath = current
      ? current.substring(0, Math.max(current.lastIndexOf('/'), current.lastIndexOf('\\')))
      : undefined
    const r = await open({ directory: false, multiple: false, ...(defaultPath ? { defaultPath } : {}) })
    if (r) onChange(typeof r === 'string' ? r : String(r))
  }, [])

  const handleSkip = useCallback(async () => {
    await saveSettings({
      whisper_cli_path: whisperCli,
      whisper_model_path: whisperModel,
      llama_cli_path: llamaCli,
      llm_model_path: llamaModel,
    })
    onDone()
  }, [whisperCli, whisperModel, llamaCli, llamaModel, saveSettings, onDone])

  const handleDone = useCallback(async () => {
    setSaving(true)
    await saveSettings({
      whisper_cli_path: whisperCli,
      whisper_model_path: whisperModel,
      llama_cli_path: llamaCli,
      llm_model_path: llamaModel,
    })
    setSaving(false)
    onDone()
  }, [whisperCli, whisperModel, llamaCli, llamaModel, saveSettings, onDone])

  const inputStyle: React.CSSProperties = {
    flex: 1, height: '32px', padding: '0 10px', boxSizing: 'border-box',
    background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
    borderRadius: '6px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none',
  }
  const browseBtn: React.CSSProperties = {
    height: '32px', padding: '0 12px', borderRadius: '6px',
    background: 'var(--color-bg-overlay)', color: 'var(--color-text-primary)',
    fontSize: '13px', border: '1px solid var(--color-border)',
    flexShrink: 0, whiteSpace: 'nowrap', cursor: 'pointer',
  }
  const sectionTitle: React.CSSProperties = {
    fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)', marginBottom: '4px',
  }
  const fieldLabel: React.CSSProperties = {
    fontSize: '12px', color: 'var(--color-text-secondary)', marginBottom: '6px',
  }

  return (
    <div style={{
      display: 'flex', flexDirection: 'column', width: '100%', height: '100%',
      background: 'var(--color-bg-base)', overflow: 'hidden',
    }}>
      {/* Header */}
      <div style={{
        padding: '28px 40px 20px',
        borderBottom: '1px solid var(--color-border)',
        flexShrink: 0,
      }}>
        <div style={{ fontSize: '22px', fontWeight: 700, color: 'var(--color-text-primary)', marginBottom: '6px' }}>
          初始設定
        </div>
        <div style={{ fontSize: '13px', color: 'var(--color-text-secondary)', lineHeight: 1.6 }}>
          在開始使用前，請設定語音辨識（Whisper）和語言模型（LLaMA）所需的執行檔與模型。<br />
          可立即下載，或稍後在「設定」中完成。
        </div>
      </div>

      {/* Scrollable content */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '24px 40px' }}>

        {/* ── Whisper section ────────────────────────────────── */}
        <div style={{ marginBottom: '32px' }}>
          <div style={sectionTitle}>語音辨識（Whisper）</div>
          <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', margin: '0 0 16px', lineHeight: 1.5 }}>
            用於語音輸入與轉錄功能。
          </p>

          <div style={{ marginBottom: '14px' }}>
            <div style={fieldLabel}>Whisper 執行檔路徑</div>
            <div style={{ display: 'flex', gap: '8px' }}>
              <input
                value={whisperCli}
                onChange={e => setWhisperCli(e.target.value)}
                placeholder="選擇 whisper-server 執行檔…"
                style={inputStyle}
              />
              <button style={browseBtn} onClick={() => pickFile(setWhisperCli, whisperCli)}>
                瀏覽
              </button>
            </div>
          </div>

          <ModelDownloader
            models={WHISPER_MODELS}
            title="Whisper 語音模型"
            kind="whisper"
            value={whisperModel}
            onChange={setWhisperModel}
          />
        </div>

        {/* ── LLaMA section ──────────────────────────────────── */}
        <div style={{ marginBottom: '32px' }}>
          <div style={sectionTitle}>語言模型（LLaMA）</div>
          <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', margin: '0 0 16px', lineHeight: 1.5 }}>
            用於 AI 對話與 Agent 功能。
          </p>

          <div style={{ marginBottom: '14px' }}>
            <div style={fieldLabel}>LLaMA 執行檔路徑</div>
            <div style={{ display: 'flex', gap: '8px' }}>
              <input
                value={llamaCli}
                onChange={e => setLlamaCli(e.target.value)}
                placeholder="選擇 llama-server 執行檔…"
                style={inputStyle}
              />
              <button style={browseBtn} onClick={() => pickFile(setLlamaCli, llamaCli)}>
                瀏覽
              </button>
            </div>
          </div>

          <ModelDownloader
            models={LLM_MODELS}
            title="語言模型"
            kind="llm"
            value={llamaModel}
            onChange={setLlamaModel}
          />
        </div>
      </div>

      {/* Footer */}
      <div style={{
        padding: '16px 40px',
        borderTop: '1px solid var(--color-border)',
        display: 'flex', alignItems: 'center', justifyContent: 'space-between',
        flexShrink: 0, background: 'var(--color-bg-base)',
      }}>
        <button
          onClick={handleSkip}
          style={{
            padding: '8px 20px', borderRadius: '8px', fontSize: '13px',
            background: 'transparent', color: 'var(--color-text-muted)',
            border: '1px solid var(--color-border)', cursor: 'pointer',
          }}
        >
          稍後設定
        </button>

        <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
          {!allSet && (
            <span style={{ fontSize: '12px', color: 'var(--color-text-muted)' }}>
              仍有未設定項目
            </span>
          )}
          <button
            onClick={handleDone}
            disabled={saving}
            style={{
              padding: '8px 24px', borderRadius: '8px', fontSize: '13px', fontWeight: 600,
              background: allSet ? 'var(--color-accent)' : 'var(--color-bg-overlay)',
              color: allSet ? '#fff' : 'var(--color-text-muted)',
              border: 'none', cursor: saving ? 'wait' : 'pointer',
              opacity: saving ? 0.6 : 1, transition: 'all 0.15s',
            }}
          >
            {saving ? '儲存中…' : '完成設定'}
          </button>
        </div>
      </div>
    </div>
  )
}
