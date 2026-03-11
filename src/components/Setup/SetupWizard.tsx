import { useState, useEffect, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useSettingsStore } from '../../stores/settingsStore'
import ModelDownloader, { WHISPER_MODELS, LLM_MODELS } from '../Settings/ModelDownloader'
import { toast } from '../common/Toast'

function fmtBytes(bytes: number): string {
  if (bytes === 0) return '0 B'
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`
}

function fmtSpeed(bps: number): string {
  if (bps < 1024 * 1024) return `${(bps / 1024).toFixed(0)} KB/s`
  return `${(bps / 1024 / 1024).toFixed(1)} MB/s`
}

interface Props {
  onDone: () => void
}

export default function SetupWizard({ onDone }: Props) {
  const { settings, saveSystem } = useSettingsStore()

  const [whisperModel, setWhisperModel] = useState(settings.whisper_model_path)
  const [llamaModel, setLlamaModel] = useState(settings.llm_model_path)
  const [whisperCli, setWhisperCli] = useState(settings.whisper_cli_path)
  const [llamaCli, setLlamaCli] = useState(settings.llama_cli_path)
  const [saving, setSaving] = useState(false)

  // ── Whisper binary download state ─────────────────────────────────────────
  const [whisperDownloading, setWhisperDownloading] = useState(false)
  const [whisperDownloadedBytes, setWhisperDownloadedBytes] = useState(0)
  const [whisperTotalBytes, setWhisperTotalBytes] = useState(0)
  const [whisperSpeedBps, setWhisperSpeedBps] = useState(0)
  const [whisperInstalled, setWhisperInstalled] = useState(false)

  // ── LLaMA binary download state ────────────────────────────────────────────
  const [llamaDownloading, setLlamaDownloading] = useState(false)
  const [llamaDownloadedBytes, setLlamaDownloadedBytes] = useState(0)
  const [llamaTotalBytes, setLlamaTotalBytes] = useState(0)
  const [llamaSpeedBps, setLlamaSpeedBps] = useState(0)
  const [llamaInstalled, setLlamaInstalled] = useState(false)

  const allSet = !!whisperCli && !!whisperModel && !!llamaCli && !!llamaModel

  // Check if binaries already installed
  useEffect(() => {
    invoke<string | null>('get_whisper_binary_path').then(p => {
      if (p) { setWhisperInstalled(true); setWhisperCli(p) }
    }).catch(() => {})
    invoke<string | null>('get_llama_binary_path').then(p => {
      if (p) { setLlamaInstalled(true); setLlamaCli(p) }
    }).catch(() => {})
  }, [])

  // Listen for whisper binary download progress
  useEffect(() => {
    let unlisten: (() => void) | null = null
    let cancelled = false
    listen<any>('model-download-progress', (e) => {
      if (e.payload.model_id !== '__whisper_server__') return
      const p = e.payload
      if (p.status === 'downloading') {
        setWhisperDownloadedBytes(p.downloaded_bytes ?? 0)
        setWhisperTotalBytes(p.total_bytes ?? 0)
        setWhisperSpeedBps(p.speed_bps ?? 0)
      } else if (p.status === 'completed') {
        setWhisperDownloading(false)
        setWhisperDownloadedBytes(0); setWhisperTotalBytes(0); setWhisperSpeedBps(0)
        setWhisperInstalled(true)
        if (p.file_path) setWhisperCli(p.file_path)
        toast.success('whisper-server 下載完成，路徑已自動填入')
      } else if (p.status === 'error') {
        setWhisperDownloading(false)
        setWhisperDownloadedBytes(0); setWhisperTotalBytes(0); setWhisperSpeedBps(0)
        toast.error('下載失敗：' + p.error)
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [])

  // Listen for llama binary download progress
  useEffect(() => {
    let unlisten: (() => void) | null = null
    let cancelled = false
    listen<any>('model-download-progress', (e) => {
      if (e.payload.model_id !== '__llama_server__') return
      const p = e.payload
      if (p.status === 'downloading') {
        setLlamaDownloadedBytes(p.downloaded_bytes ?? 0)
        setLlamaTotalBytes(p.total_bytes ?? 0)
        setLlamaSpeedBps(p.speed_bps ?? 0)
      } else if (p.status === 'completed') {
        setLlamaDownloading(false)
        setLlamaDownloadedBytes(0); setLlamaTotalBytes(0); setLlamaSpeedBps(0)
        setLlamaInstalled(true)
        if (p.file_path) setLlamaCli(p.file_path)
        toast.success('llama-server 下載完成，路徑已自動填入')
      } else if (p.status === 'error') {
        setLlamaDownloading(false)
        setLlamaDownloadedBytes(0); setLlamaTotalBytes(0); setLlamaSpeedBps(0)
        toast.error('下載失敗：' + p.error)
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [])

  const handleSave = useCallback(async () => {
    setSaving(true)
    await saveSystem({
      whisper_cli_path: whisperCli,
      whisper_model_path: whisperModel,
      llama_cli_path: llamaCli,
      llm_model_path: llamaModel,
    } as any)
    setSaving(false)
    onDone()
  }, [whisperCli, whisperModel, llamaCli, llamaModel, saveSystem, onDone])

  const handleSkip = useCallback(async () => {
    await saveSystem({
      whisper_cli_path: whisperCli,
      whisper_model_path: whisperModel,
      llama_cli_path: llamaCli,
      llm_model_path: llamaModel,
    } as any)
    onDone()
  }, [whisperCli, whisperModel, llamaCli, llamaModel, saveSystem, onDone])

  // ── Styles ─────────────────────────────────────────────────────────────────
  const sectionTitle: React.CSSProperties = {
    fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)', marginBottom: '4px',
  }
  const fieldLabel: React.CSSProperties = {
    fontSize: '12px', color: 'var(--color-text-secondary)', marginBottom: '6px', fontWeight: 500,
  }
  const downloadBtn = (disabled: boolean): React.CSSProperties => ({
    width: '100%', height: '32px', padding: '0 10px', boxSizing: 'border-box',
    background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
    borderRadius: '6px', color: 'var(--color-text-primary)', fontSize: '13px',
    outline: 'none', textAlign: 'left',
    cursor: disabled ? 'not-allowed' : 'pointer',
    opacity: disabled ? 0.6 : 1,
  })

  const ProgressBar = ({ downloaded, total, speed }: { downloaded: number; total: number; speed: number }) => (
    <div style={{ marginTop: '8px' }}>
      <div style={{ height: '4px', borderRadius: '2px', background: 'var(--color-bg-overlay)', overflow: 'hidden' }}>
        <div style={{
          height: '100%',
          width: total > 0 ? `${Math.round(downloaded / total * 100)}%` : '100%',
          background: 'var(--color-accent)', borderRadius: '2px',
          transition: total > 0 ? 'width 0.3s ease' : undefined,
          animation: total === 0 ? 'pulse 1.5s ease-in-out infinite' : undefined,
        }} />
      </div>
      <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '4px' }}>
        <span>{fmtBytes(downloaded)}{total > 0 ? ` / ${fmtBytes(total)}` : ''}</span>
        <span>
          {total > 0 ? `${Math.round(downloaded / total * 100)}%` : '連線中…'}
          {speed > 0 ? ` · ${fmtSpeed(speed)}` : ''}
        </span>
      </div>
    </div>
  )

  return (
    <div style={{ display: 'flex', flexDirection: 'column', width: '100%', height: '100%', background: 'var(--color-bg-base)', overflow: 'hidden' }}>

      {/* Header */}
      <div style={{ padding: '28px 40px 20px', borderBottom: '1px solid var(--color-border)', flexShrink: 0 }}>
        <div style={{ fontSize: '22px', fontWeight: 700, color: 'var(--color-text-primary)', marginBottom: '6px' }}>
          初始設定
        </div>
        <div style={{ fontSize: '13px', color: 'var(--color-text-secondary)', lineHeight: 1.6 }}>
          在開始使用前，請下載語音辨識（Whisper）和語言模型（LLaMA）所需的執行檔與模型。<br />
          可立即下載，或稍後在「設定」中完成。
        </div>
      </div>

      {/* Scrollable content */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '24px 40px' }}>

        {/* ── Whisper section ──────────────────────────────────────────── */}
        <div style={{ marginBottom: '32px' }}>
          <div style={sectionTitle}>語音辨識（Whisper）</div>
          <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', margin: '0 0 16px', lineHeight: 1.5 }}>
            用於語音輸入與轉錄功能。
          </p>

          {/* Whisper binary download */}
          <div style={{ marginBottom: '16px' }}>
            <div style={fieldLabel}>
              Whisper 執行檔
              {whisperInstalled && whisperCli && (
                <span style={{ marginLeft: '8px', fontSize: '11px', color: 'var(--color-success)', fontWeight: 400 }}>已安裝 ✓</span>
              )}
            </div>
            <button
              onClick={() => {
                setWhisperDownloading(true)
                invoke('download_whisper_server').catch((e: any) => {
                  setWhisperDownloading(false)
                  toast.error('下載失敗：' + (e?.Import ?? String(e)))
                })
              }}
              disabled={whisperDownloading}
              style={downloadBtn(whisperDownloading)}
            >
              {whisperDownloading ? '下載中…' : whisperInstalled ? '重新安裝最新版' : '自動安裝最新版（Metal）'}
            </button>
            {whisperDownloading && (
              <ProgressBar downloaded={whisperDownloadedBytes} total={whisperTotalBytes} speed={whisperSpeedBps} />
            )}
            {whisperCli && !whisperDownloading && (
              <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                路徑：{whisperCli}
              </p>
            )}
            <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
              從 noteTreeLM Releases 下載預建版本（macOS ARM 含 Metal 加速）。
            </p>
          </div>

          {/* Whisper model */}
          <ModelDownloader
            models={WHISPER_MODELS}
            title="Whisper 語音模型"
            kind="whisper"
            value={whisperModel}
            onChange={setWhisperModel}
          />
        </div>

        {/* ── LLaMA section ────────────────────────────────────────────── */}
        <div style={{ marginBottom: '32px' }}>
          <div style={sectionTitle}>語言模型（LLaMA）</div>
          <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', margin: '0 0 16px', lineHeight: 1.5 }}>
            用於 AI 對話與 Agent 功能。
          </p>

          {/* LLaMA binary download */}
          <div style={{ marginBottom: '16px' }}>
            <div style={fieldLabel}>
              LLaMA 執行檔
              {llamaInstalled && llamaCli && (
                <span style={{ marginLeft: '8px', fontSize: '11px', color: 'var(--color-success)', fontWeight: 400 }}>已安裝 ✓</span>
              )}
            </div>
            <button
              onClick={() => {
                setLlamaDownloading(true)
                invoke('download_llama_server').catch((e: any) => {
                  setLlamaDownloading(false)
                  toast.error('下載失敗：' + (e?.Import ?? String(e)))
                })
              }}
              disabled={llamaDownloading}
              style={downloadBtn(llamaDownloading)}
            >
              {llamaDownloading ? '下載中…' : llamaInstalled ? '重新安裝最新版' : '自動安裝最新版'}
            </button>
            {llamaDownloading && (
              <ProgressBar downloaded={llamaDownloadedBytes} total={llamaTotalBytes} speed={llamaSpeedBps} />
            )}
            {llamaCli && !llamaDownloading && (
              <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                路徑：{llamaCli}
              </p>
            )}
            <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
              從 ggerganov/llama.cpp 官方 Releases 下載預建版本（macOS ARM / Windows AVX2）。
            </p>
          </div>

          {/* LLaMA model */}
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
            onClick={handleSave}
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
