import { useState, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { open } from '@tauri-apps/plugin-dialog'
import { useSettingsStore } from '../../stores/settingsStore'
import { useVaultStore } from '../../stores/vaultStore'
import { useGraphStore } from '../../stores/graphStore'
import { useEditorStore } from '../../stores/editorStore'
import { Settings, DEFAULT_SETTINGS } from '../../types/settings'
import ModelDownloader, { WHISPER_MODELS, LLM_MODELS } from './ModelDownloader'
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

interface SettingsModalProps {
  onClose: () => void
}

interface MemoryRuleEntry {
  id: number
  pattern_type: string
  pattern: string
  value: string
  created_at: number
}

type Tab = 'general' | 'ai' | 'voice' | 'local' | 'advanced' | 'raw'
type ServerStatus = 'unknown' | 'running' | 'loading' | 'stopped'

// Provider → 預設模型清單
const MODEL_OPTIONS: Record<string, string[]> = {
  openai:    ['gpt-4o', 'gpt-4o-mini', 'gpt-4-turbo', 'gpt-4', 'gpt-3.5-turbo'],
  anthropic: ['claude-opus-4-6', 'claude-sonnet-4-6', 'claude-haiku-4-5-20251001'],
  ollama:    ['llama3.2', 'llama3.1', 'mistral', 'codestral', 'gemma2', 'phi3'],
}
const DEFAULT_MODEL: Record<string, string> = {
  openai: 'gpt-4o', anthropic: 'claude-sonnet-4-6', ollama: 'llama3.2',
}
const DEFAULT_BASE_URL: Record<string, string> = {
  openai: 'https://api.openai.com/v1',
  anthropic: 'https://api.anthropic.com/v1',
  ollama: 'http://localhost:11434/v1',
}

const labelStyle: React.CSSProperties = {
  display: 'block', fontSize: '12px', color: 'var(--color-text-secondary)', marginBottom: '6px',
}
const fieldStyle: React.CSSProperties = { marginBottom: '18px' }

export default function SettingsModal({ onClose }: SettingsModalProps) {
  const { settings, save, getApiKey, setApiKey } = useSettingsStore()
  const { scanVault } = useVaultStore()
  const { load: loadGraph } = useGraphStore()
  const { setCurrentPath } = useEditorStore()

  const [tab, setTab] = useState<Tab>('general')
  const [draft, setDraft] = useState<Settings>({ ...settings })
  const [apiKey, setApiKeyLocal] = useState('')
  const [apiKeySaved, setApiKeySaved] = useState(false)
  const [isSaving, setIsSaving] = useState(false)

  const [whisperStatus, setWhisperStatus] = useState<ServerStatus>('unknown')
  const [llamaStatus, setLlamaStatus] = useState<ServerStatus>('unknown')
  const [whisperBusy, setWhisperBusy] = useState(false)
  const [llamaBusy, setLlamaBusy] = useState(false)
  const [binaryDownloading, setBinaryDownloading] = useState(false)
  const [binaryDownloadedBytes, setBinaryDownloadedBytes] = useState(0)
  const [binaryTotalBytes, setBinaryTotalBytes] = useState(0)
  const [binarySpeedBps, setBinarySpeedBps] = useState(0)
  const [binaryInstalled, setBinaryInstalled] = useState(false)

  const [llamaBinaryDownloading, setLlamaBinaryDownloading] = useState(false)
  const [llamaBinaryDownloadedBytes, setLlamaBinaryDownloadedBytes] = useState(0)
  const [llamaBinaryTotalBytes, setLlamaBinaryTotalBytes] = useState(0)
  const [llamaBinarySpeedBps, setLlamaBinarySpeedBps] = useState(0)
  const [llamaBinaryInstalled, setLlamaBinaryInstalled] = useState(false)

  const [memoryRules, setMemoryRules] = useState<MemoryRuleEntry[]>([])
  const [memoryRulesLoading, setMemoryRulesLoading] = useState(false)

  const colorScheme = draft.theme === 'dark' ? 'dark' : 'light'
  const inputStyle: React.CSSProperties = {
    width: '100%', height: '32px', padding: '0 10px', boxSizing: 'border-box',
    background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
    borderRadius: '6px', color: 'var(--color-text-primary)', fontSize: '13px',
    outline: 'none', colorScheme,
  }
  const disabledStyle: React.CSSProperties = {
    ...inputStyle, opacity: 0.4, cursor: 'not-allowed', pointerEvents: 'none',
  }

  useEffect(() => {
    if (draft.ai_provider)
      getApiKey(draft.ai_provider).then((k) => setApiKeyLocal(k || ''))
    else
      setApiKeyLocal('')
  }, [draft.ai_provider])

  useEffect(() => {
    if (tab !== 'voice' && tab !== 'local') return
    const refresh = async () => {
      const ws = await invoke<string>('get_whisper_server_status').catch(() => 'stopped')
      setWhisperStatus(ws as ServerStatus)
      const ls = await invoke<string>('get_llama_server_status').catch(() => 'stopped')
      setLlamaStatus(ls as ServerStatus)
    }
    refresh()
    const id = setInterval(refresh, 3000)
    return () => clearInterval(id)
  }, [tab])

  // 檢查 whisper-server binary 是否已安裝
  useEffect(() => {
    if (tab !== 'voice') return
    invoke<string | null>('get_whisper_binary_path').then(p => setBinaryInstalled(!!p))
  }, [tab])

  // 監聽 whisper-server binary 下載進度
  useEffect(() => {
    let unlisten: (() => void) | null = null
    let cancelled = false
    listen<any>('model-download-progress', (e) => {
      if (e.payload.model_id !== '__whisper_server__') return
      const p = e.payload
      if (p.status === 'downloading') {
        setBinaryDownloadedBytes(p.downloaded_bytes ?? 0)
        setBinaryTotalBytes(p.total_bytes ?? 0)
        setBinarySpeedBps(p.speed_bps ?? 0)
      } else if (p.status === 'completed') {
        setBinaryDownloading(false)
        setBinaryDownloadedBytes(0)
        setBinaryTotalBytes(0)
        setBinarySpeedBps(0)
        setBinaryInstalled(true)
        if (p.file_path) up({ whisper_cli_path: p.file_path })
        toast.success('whisper-server 下載完成，路徑已自動填入')
      } else if (p.status === 'error') {
        setBinaryDownloading(false)
        setBinaryDownloadedBytes(0)
        setBinaryTotalBytes(0)
        setBinarySpeedBps(0)
        toast.error('下載失敗：' + p.error)
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [])

  // 檢查 llama-server binary 是否已安裝
  useEffect(() => {
    if (tab !== 'local') return
    invoke<string | null>('get_llama_binary_path').then(p => setLlamaBinaryInstalled(!!p))
  }, [tab])

  // 監聽 llama-server binary 下載進度
  useEffect(() => {
    let unlisten: (() => void) | null = null
    let cancelled = false
    listen<any>('model-download-progress', (e) => {
      if (e.payload.model_id !== '__llama_server__') return
      const p = e.payload
      if (p.status === 'downloading') {
        setLlamaBinaryDownloadedBytes(p.downloaded_bytes ?? 0)
        setLlamaBinaryTotalBytes(p.total_bytes ?? 0)
        setLlamaBinarySpeedBps(p.speed_bps ?? 0)
      } else if (p.status === 'completed') {
        setLlamaBinaryDownloading(false)
        setLlamaBinaryDownloadedBytes(0)
        setLlamaBinaryTotalBytes(0)
        setLlamaBinarySpeedBps(0)
        setLlamaBinaryInstalled(true)
        if (p.file_path) up({ llama_cli_path: p.file_path })
        toast.success('llama-server 下載完成，路徑已自動填入')
      } else if (p.status === 'error') {
        setLlamaBinaryDownloading(false)
        setLlamaBinaryDownloadedBytes(0)
        setLlamaBinaryTotalBytes(0)
        setLlamaBinarySpeedBps(0)
        toast.error('下載失敗：' + p.error)
      }
    }).then(fn => { if (cancelled) fn(); else unlisten = fn })
    return () => { cancelled = true; unlisten?.() }
  }, [])

  useEffect(() => {
    if (tab !== 'local') return
    setMemoryRulesLoading(true)
    invoke<MemoryRuleEntry[]>('get_memory_rules')
      .then(setMemoryRules)
      .catch(() => setMemoryRules([]))
      .finally(() => setMemoryRulesLoading(false))
  }, [tab])

  const handleDeleteMemoryRule = async (id: number) => {
    await invoke('delete_memory_rule', { id }).catch(() => {})
    setMemoryRules((prev) => prev.filter((r) => r.id !== id))
  }

  const patternTypeLabel = (pt: string) => {
    if (pt === 'temporal_exact_days') return '固定天數'
    if (pt === 'temporal_unit') return '時間單位'
    if (pt === 'stopword') return '停用詞'
    return pt
  }

  const up = (partial: Partial<Settings>) => setDraft((d) => ({ ...d, ...partial }))

  const handleProviderChange = (provider: string) => {
    up({
      ai_provider: provider,
      ai_model: DEFAULT_MODEL[provider] ?? '',
      ai_base_url: DEFAULT_BASE_URL[provider] ?? '',
    })
  }

  const handleSave = async () => {
    setIsSaving(true)
    try {
      const vaultChanged = draft.vault_path !== settings.vault_path
      await save(draft)
      document.documentElement.setAttribute('data-theme', draft.theme)
      if (draft.font_sans)
        document.documentElement.style.setProperty('--font-sans', draft.font_sans)
      else
        document.documentElement.style.removeProperty('--font-sans')
      document.documentElement.style.setProperty(
        '--font-mono',
        draft.font_mono || "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace"
      )
      document.documentElement.style.setProperty('--font-size-editor', `${draft.editor_font_size || 14}px`)
      document.documentElement.style.zoom = String((draft.ui_font_size || 14) / 14)
      if (vaultChanged) {
        setCurrentPath('')  // 關閉目前開啟的筆記，避免對新 vault 執行舊路徑操作
        await scanVault()
        await loadGraph()
      }
      onClose()
    } catch (err: any) {
      const msg = err?.Settings ?? err?.message ?? (typeof err === 'string' ? err : '未知錯誤')
      toast.error('設定儲存失敗：' + msg)
    } finally {
      setIsSaving(false)
    }
  }

  const handleReset = () => {
    setDraft({
      ...DEFAULT_SETTINGS,
      vault_path: settings.vault_path,
      onboarding_done: settings.onboarding_done,
      last_open_note: settings.last_open_note,
      recent_vaults: settings.recent_vaults,
      sidebar_width: settings.sidebar_width,
      graph_panel_width: settings.graph_panel_width,
    })
  }

  const handleSaveApiKey = async () => {
    await setApiKey(draft.ai_provider, apiKey)
    setApiKeySaved(true)
    setTimeout(() => setApiKeySaved(false), 2000)
  }

  // ── Sub-components ──────────────────────────────────────────────

  const PathPicker = ({ value, onChange, isDir = false, disabled = false }: { value: string; onChange: (v: string) => void; isDir?: boolean; disabled?: boolean }) => (
    <div style={{ display: 'flex', gap: '8px' }}>
      <input value={value} onChange={(e) => onChange(e.target.value)}
        placeholder={isDir ? '選擇資料夾…' : '選擇檔案…'}
        disabled={disabled}
        style={{ ...(disabled ? disabledStyle : inputStyle), flex: 1 }} />
      <button
        disabled={disabled}
        onClick={async () => {
          const r = await open({ directory: isDir, multiple: false })
          if (r) onChange(typeof r === 'string' ? r : String(r))
        }}
        style={{ height: '32px', padding: '0 12px', borderRadius: '6px', background: 'var(--color-bg-overlay)', color: disabled ? 'var(--color-text-muted)' : 'var(--color-text-primary)', fontSize: '13px', border: '1px solid var(--color-border)', flexShrink: 0, whiteSpace: 'nowrap', opacity: disabled ? 0.4 : 1, cursor: disabled ? 'not-allowed' : 'pointer' }}
      >瀏覽</button>
    </div>
  )

  const Toggle = ({ value, onChange }: { value: boolean; onChange: (v: boolean) => void }) => (
    <div onClick={() => onChange(!value)} style={{
      width: '40px', height: '22px', borderRadius: '11px',
      background: value ? 'var(--color-accent)' : 'var(--color-bg-overlay)',
      cursor: 'pointer', position: 'relative', transition: 'background 0.2s', flexShrink: 0,
    }}>
      <div style={{
        position: 'absolute', top: '3px', left: value ? '21px' : '3px',
        width: '16px', height: '16px', borderRadius: '50%',
        background: '#fff', transition: 'left 0.2s',
      }} />
    </div>
  )

  const ToggleRow = ({ label, value, onChange }: { label: string; value: boolean; onChange: (v: boolean) => void }) => (
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: '16px' }}>
      <span style={{ fontSize: '13px', color: 'var(--color-text-primary)' }}>{label}</span>
      <Toggle value={value} onChange={onChange} />
    </div>
  )

  const SectionHeader = ({ label, locked }: { label: string; locked?: boolean }) => (
    <div style={{ display: 'flex', alignItems: 'center', gap: '8px', marginBottom: '12px' }}>
      <span style={{
        fontSize: '10.5px', fontWeight: 600, letterSpacing: '0.07em',
        color: 'var(--color-text-secondary)', textTransform: 'uppercase' as const,
      }}>{label}</span>
      {locked && (
        <span style={{
          fontSize: '10px', color: 'var(--color-warning, #f59e0b)',
          background: 'rgba(245,158,11,0.1)', border: '1px solid rgba(245,158,11,0.25)',
          padding: '1px 7px', borderRadius: '4px', fontWeight: 500,
        }}>執行中・已鎖定</span>
      )}
    </div>
  )

  const SectionDivider = () => (
    <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0 20px' }} />
  )

  const ServerCard = ({
    name, status, busy, onStart, onStop, onRestart,
  }: {
    name: string; status: ServerStatus; busy: boolean
    onStart: () => void; onStop: () => void; onRestart: () => void
  }) => {
    const dotColor = status === 'running' ? '#4ec9b0' : status === 'loading' ? '#d19a66' : status === 'stopped' ? '#e06c75' : '#666'
    const statusLabel = status === 'running' ? '已啟動' : status === 'loading' ? '載入中…' : status === 'stopped' ? '已停止' : '偵測中'
    const actionBtn = (label: string, color: string, onClick: () => void) => (
      <button onClick={onClick} style={{
        padding: '5px 14px', borderRadius: '6px', fontSize: '12px', cursor: 'pointer',
        background: `${color}22`, border: `1px solid ${color}`, color,
      }}>{label}</button>
    )
    return (
      <div style={{ border: '1px solid var(--color-border)', borderRadius: '8px', padding: '16px', marginBottom: '16px' }}>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
            <span style={{ display: 'inline-block', width: 10, height: 10, borderRadius: '50%', background: dotColor, flexShrink: 0 }} />
            <div>
              <div style={{ fontSize: '13px', fontWeight: 500, color: 'var(--color-text-primary)' }}>{name}</div>
              <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginTop: 2 }}>
                {busy ? '操作中…' : statusLabel}
              </div>
            </div>
          </div>
          {!busy && (
            <div style={{ display: 'flex', gap: '8px' }}>
              {status === 'running' && <>{actionBtn('停止', '#e06c75', onStop)}{actionBtn('重啟', '#d19a66', onRestart)}</>}
              {status === 'loading' && actionBtn('強制停止', '#e06c75', onStop)}
              {status === 'stopped' && actionBtn('啟動', '#4ec9b0', onStart)}
            </div>
          )}
        </div>
      </div>
    )
  }

  // ── Render ───────────────────────────────────────────────────────

  const hasProvider = !!draft.ai_provider
  const modelOptions = MODEL_OPTIONS[draft.ai_provider] ?? []
  const modelIsCustom = hasProvider && !modelOptions.includes(draft.ai_model) && draft.ai_model

  const tabs: [Tab, string][] = [
    ['general', '一般'], ['ai', '外部資源'], ['voice', 'Whisper'], ['local', 'Local LLM'], ['advanced', '進階'], ['raw', '設定檔'],
  ]

  const numInputStyle: React.CSSProperties = {
    ...inputStyle,
    // Reserve space for spin buttons so the right gap matches a <select>'s arrow area
    paddingRight: '4px',
  }

  return (
    <div
      style={{
        position: 'fixed', inset: 0, zIndex: 1000,
        background: 'rgba(0,0,0,0.5)',
        display: 'flex', alignItems: 'center', justifyContent: 'center',
      }}
      onClick={(e) => { if (e.target === e.currentTarget) onClose() }}
    >
      {/* Scope spin-button margin so it only affects this modal */}
      <style>{`
        #settings-modal input[type=number]::-webkit-inner-spin-button,
        #settings-modal input[type=number]::-webkit-outer-spin-button {
          margin-left: 4px;
          opacity: 1;
        }
      `}</style>
      <div id="settings-modal" style={{
        width: 620, height: 540,
        borderRadius: '12px',
        background: 'var(--color-bg-surface)', border: '1px solid var(--color-border)',
        boxShadow: 'var(--shadow-lg)',
        display: 'flex', flexDirection: 'column',
        overflow: 'hidden',
      }}>
        {/* Header */}
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '14px 20px', borderBottom: '1px solid var(--color-border)', flexShrink: 0 }}>
          <span style={{ fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)' }}>設定</span>
          <button onClick={onClose}
            style={{ color: 'var(--color-text-secondary)', fontSize: '20px', lineHeight: 1, width: '28px', height: '28px', display: 'flex', alignItems: 'center', justifyContent: 'center', borderRadius: '4px' }}
            onMouseEnter={(e) => (e.currentTarget.style.color = 'var(--color-text-primary)')}
            onMouseLeave={(e) => (e.currentTarget.style.color = 'var(--color-text-secondary)')}
          >×</button>
        </div>

        {/* Body */}
        <div style={{ display: 'flex', flex: 1, overflow: 'hidden' }}>
          {/* Tab sidebar */}
          <div style={{ width: '110px', borderRight: '1px solid var(--color-border)', padding: '8px 0', flexShrink: 0, overflowY: 'auto', background: 'var(--color-bg-elevated)' }}>
            {tabs.map(([t, label]) => (
              <div key={t} onClick={() => setTab(t)} style={{
                padding: '8px 16px', fontSize: '13px', cursor: 'pointer',
                color: tab === t ? 'var(--color-text-primary)' : 'var(--color-text-secondary)',
                background: tab === t ? 'var(--color-accent-dim)' : 'transparent',
                borderRight: `2px solid ${tab === t ? 'var(--color-accent)' : 'transparent'}`,
                transition: 'all 0.1s',
              }}>{label}</div>
            ))}
          </div>

          {/* Tab content */}
          <div style={{ flex: 1, overflowY: 'auto', padding: '20px 24px' }}>

            {tab === 'general' && <>
              <div style={fieldStyle}>
                <label style={labelStyle}>佈景主題</label>
                <select value={draft.theme} onChange={(e) => up({ theme: e.target.value as any })} style={inputStyle}>
                  <option value="dark">深色</option>
                  <option value="light">淺色</option>
                </select>
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>自動儲存</label>
                <select value={draft.auto_save_mode} onChange={(e) => up({ auto_save_mode: e.target.value as any })} style={inputStyle}>
                  <option value="off">關閉</option>
                  <option value="afterDelay">延遲後儲存</option>
                  <option value="onFocusChange">失去焦點時</option>
                  <option value="onWindowChange">切換視窗時</option>
                </select>
              </div>
              {draft.auto_save_mode === 'afterDelay' && (
                <div style={fieldStyle}>
                  <label style={labelStyle}>延遲時間（毫秒）</label>
                  <input type="number" min={500} max={10000} step={500}
                    value={draft.auto_save_delay}
                    onChange={(e) => up({ auto_save_delay: Number(e.target.value) })}
                    style={numInputStyle} />
                </div>
              )}
              <div style={fieldStyle}>
                <label style={labelStyle}>介面字型</label>
                <select value={draft.font_sans} onChange={(e) => up({ font_sans: e.target.value })} style={inputStyle}>
                  <option value="">系統預設（Inter）</option>
                  <option value="'-apple-system', 'Helvetica Neue', sans-serif">macOS 系統字型</option>
                  <option value="'Segoe UI', sans-serif">Segoe UI（Windows）</option>
                  <option value="'Georgia', serif">Georgia（襯線）</option>
                </select>
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>編輯器字型</label>
                <select value={draft.font_mono} onChange={(e) => up({ font_mono: e.target.value })} style={inputStyle}>
                  <option value="">系統預設（JetBrains Mono）</option>
                  <option value="'Menlo', monospace">Menlo（macOS）</option>
                  <option value="'Consolas', monospace">Consolas（Windows）</option>
                  <option value="'Fira Code', monospace">Fira Code</option>
                  <option value="'Cascadia Code', monospace">Cascadia Code</option>
                  <option value="'Courier New', monospace">Courier New</option>
                </select>
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>編輯器字級（px）</label>
                <select value={draft.editor_font_size} onChange={(e) => up({ editor_font_size: Number(e.target.value) })} style={inputStyle}>
                  <option value={12}>12px</option>
                  <option value={13}>13px</option>
                  <option value={14}>14px（預設）</option>
                  <option value={15}>15px</option>
                  <option value={16}>16px</option>
                  <option value={18}>18px</option>
                  <option value={20}>20px</option>
                </select>
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>介面字級（px）</label>
                <select value={draft.ui_font_size} onChange={(e) => up({ ui_font_size: Number(e.target.value) })} style={inputStyle}>
                  <option value={12}>12px（緊湊）</option>
                  <option value={13}>13px</option>
                  <option value={14}>14px（預設）</option>
                  <option value={15}>15px</option>
                  <option value={16}>16px（寬鬆）</option>
                </select>
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>圖譜節點字級（px）</label>
                <select value={draft.graph_font_size} onChange={(e) => up({ graph_font_size: Number(e.target.value) })} style={inputStyle}>
                  <option value={9}>9px（極小）</option>
                  <option value={10}>10px</option>
                  <option value={11}>11px（預設）</option>
                  <option value={12}>12px</option>
                  <option value={13}>13px</option>
                  <option value={14}>14px</option>
                </select>
              </div>
            </>}

            {tab === 'ai' && <>
              <div style={fieldStyle}>
                <label style={labelStyle}>外部 AI 提供商</label>
                <select value={draft.ai_provider} onChange={(e) => handleProviderChange(e.target.value)} style={inputStyle}>
                  <option value="">未設定</option>
                  <option value="openai">OpenAI</option>
                  <option value="anthropic">Anthropic</option>
                  <option value="ollama">Ollama（本地 API）</option>
                </select>
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '5px 0 0', lineHeight: 1.5 }}>
                  當本地工具（記憶查詢、主題分析等）無法回答時，Chat 會透過此提供商發送請求作為外部輔助。本地 LLM 路徑與模型請至「Local LLM」頁面設定。
                </p>
              </div>

              {/* 提供商欄位：未選擇時隱藏 */}
              {draft.ai_provider && <>
                {/* 模型 — 下拉選單（依提供商），Ollama 用文字輸入 */}
                <div style={fieldStyle}>
                  <label style={labelStyle}>模型</label>
                  {!hasProvider ? (
                    <input disabled value="" placeholder="請先選擇 AI 提供商" style={disabledStyle} />
                  ) : draft.ai_provider === 'ollama' ? (
                    <input value={draft.ai_model} onChange={(e) => up({ ai_model: e.target.value })}
                      placeholder="llama3.2" style={inputStyle} />
                  ) : (
                    <select value={draft.ai_model} onChange={(e) => up({ ai_model: e.target.value })} style={inputStyle}>
                      {modelOptions.map((m) => <option key={m} value={m}>{m}</option>)}
                      {modelIsCustom && <option value={draft.ai_model}>{draft.ai_model}（自訂）</option>}
                    </select>
                  )}
                </div>

                {/* API Base URL */}
                <div style={fieldStyle}>
                  <label style={labelStyle}>API Base URL</label>
                  <input
                    value={draft.ai_base_url}
                    disabled={!hasProvider}
                    onChange={(e) => up({ ai_base_url: e.target.value })}
                    placeholder="https://api.openai.com/v1"
                    style={hasProvider ? inputStyle : disabledStyle}
                  />
                </div>

                {/* API Key */}
                <div style={fieldStyle}>
                  <label style={labelStyle}>API Key</label>
                  <div style={{ display: 'flex', gap: '8px' }}>
                    <input
                      type="password"
                      value={apiKey}
                      disabled={!hasProvider}
                      onChange={(e) => setApiKeyLocal(e.target.value)}
                      placeholder={hasProvider ? 'sk-…' : '請先選擇 AI 提供商'}
                      style={{ ...(hasProvider ? inputStyle : disabledStyle), flex: 1 }}
                    />
                    <button
                      onClick={handleSaveApiKey}
                      disabled={!hasProvider}
                      style={{
                        height: '32px', padding: '0 14px', borderRadius: '6px',
                        background: !hasProvider ? 'var(--color-bg-overlay)' : apiKeySaved ? 'var(--color-success)' : 'var(--color-accent)',
                        color: !hasProvider ? 'var(--color-text-muted)' : '#fff',
                        fontSize: '13px', flexShrink: 0, transition: 'background 0.2s',
                        cursor: hasProvider ? 'pointer' : 'not-allowed',
                      }}
                    >{apiKeySaved ? '已儲存 ✓' : '儲存 Key'}</button>
                  </div>
                </div>
              </>}

            </>}

            {tab === 'voice' && <>
              {/* ── Server Status ──────────────────────────────────────────── */}
              <ServerCard
                name="Whisper Server（語音辨識）"
                status={whisperStatus}
                busy={whisperBusy}
                onStart={async () => {
                  setWhisperBusy(true)
                  try {
                    await invoke('start_whisper_server')
                  } catch (e: any) {
                    console.error('[start_whisper_server]', e)
                    const msg = typeof e === 'string' ? e : e?.Voice ?? e?.message ?? JSON.stringify(e)
                    toast.error(msg)
                  } finally {
                    setWhisperBusy(false)
                  }
                }}
                onStop={async () => {
                  setWhisperBusy(true)
                  await invoke('stop_whisper_server').catch(() => {})
                  setWhisperStatus('stopped')
                  setWhisperBusy(false)
                }}
                onRestart={async () => {
                  setWhisperBusy(true)
                  try {
                    await invoke('restart_whisper_server')
                  } catch (e: any) {
                    console.error('[restart_whisper_server]', e)
                    const msg = typeof e === 'string' ? e : e?.Voice ?? e?.message ?? JSON.stringify(e)
                    toast.error(msg)
                  } finally {
                    setWhisperBusy(false)
                  }
                }}
              />

              {/* ── Server Configuration ───────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="伺服器設定" locked={whisperStatus === 'running'} />
              <div style={fieldStyle}>
                <label style={labelStyle}>執行檔路徑</label>
                <PathPicker value={draft.whisper_cli_path} onChange={(v) => up({ whisper_cli_path: v })} disabled={whisperStatus === 'running'} />
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                  請指向 <code>whisper-server</code> 二進位檔（第一次錄音時自動啟動）
                </p>
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>自動下載</label>
                <button
                  onClick={() => {
                    setBinaryDownloading(true)
                    invoke('download_whisper_server').catch((e: any) => {
                      setBinaryDownloading(false)
                      toast.error('下載失敗：' + (e?.Import ?? String(e)))
                    })
                  }}
                  disabled={binaryDownloading || whisperStatus === 'running'}
                  style={{ ...inputStyle, cursor: binaryDownloading || whisperStatus === 'running' ? 'not-allowed' : 'pointer', opacity: binaryDownloading || whisperStatus === 'running' ? 0.6 : 1 }}
                >
                  {binaryDownloading ? '下載中…' : binaryInstalled ? '重新安裝最新版' : '自動安裝最新版（Metal）'}
                </button>
                {binaryDownloading && (
                  <div style={{ marginTop: '8px' }}>
                    <div style={{ height: '4px', borderRadius: '2px', background: 'var(--color-bg-overlay)', overflow: 'hidden' }}>
                      <div style={{
                        height: '100%',
                        width: binaryTotalBytes > 0 ? `${Math.round(binaryDownloadedBytes / binaryTotalBytes * 100)}%` : '100%',
                        background: 'var(--color-accent)',
                        borderRadius: '2px',
                        transition: binaryTotalBytes > 0 ? 'width 0.3s ease' : undefined,
                        animation: binaryTotalBytes === 0 ? 'pulse 1.5s ease-in-out infinite' : undefined,
                      }} />
                    </div>
                    <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '4px' }}>
                      <span>
                        {fmtBytes(binaryDownloadedBytes)}
                        {binaryTotalBytes > 0 ? ` / ${fmtBytes(binaryTotalBytes)}` : ''}
                      </span>
                      <span>
                        {binaryTotalBytes > 0 ? `${Math.round(binaryDownloadedBytes / binaryTotalBytes * 100)}%` : '連線中…'}
                        {binarySpeedBps > 0 ? ` · ${fmtSpeed(binarySpeedBps)}` : ''}
                      </span>
                    </div>
                  </div>
                )}
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                  從 noteTreeLM Releases 下載預建版本（macOS ARM 含 Metal 加速）。若遇到網路問題，可手動編譯：<code>cmake … -DWHISPER_BUILD_SERVER=ON -DGGML_METAL=ON</code>
                </p>
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>CPU 執行緒數</label>
                <input
                  type="number"
                  min={1}
                  max={32}
                  value={draft.whisper_threads}
                  onChange={(e) => up({ whisper_threads: Math.max(1, parseInt(e.target.value) || 4) })}
                  style={{ ...inputStyle, width: '80px' }}
                  disabled={whisperStatus === 'running'}
                />
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                  建議設為實體核心數（預設 4）。修改後需重啟伺服器生效。
                </p>
              </div>

              {/* ── Recognition Settings ───────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="辨識設定" />
              <div style={fieldStyle}>
                <label style={labelStyle}>辨識語言</label>
                <select value={draft.whisper_language} onChange={(e) => up({ whisper_language: e.target.value })} style={inputStyle}>
                  <option value="auto">自動偵測</option>
                  <option value="zh">中文</option>
                  <option value="en">English</option>
                  <option value="ja">日本語</option>
                  <option value="ko">한국어</option>
                  <option value="fr">Français</option>
                  <option value="de">Deutsch</option>
                </select>
              </div>
              <ToggleRow label="辨識完成後自動插入編輯器" value={draft.whisper_auto_insert} onChange={(v) => up({ whisper_auto_insert: v })} />
              <div style={fieldStyle}>
                <label style={labelStyle}>語音後處理模式</label>
                <select
                  value={draft.voice_process_mode}
                  onChange={(e) => up({ voice_process_mode: e.target.value as any })}
                  style={inputStyle}
                >
                  <option value="none">無（直接插入原始文字）</option>
                  <option value="format">自動整理（llama 潤稿）</option>
                  <option value="summary">標記 Wikilink（llama 分析關鍵詞）</option>
                </select>
                {draft.voice_process_mode !== 'none' && !draft.llama_cli_path && (
                  <p style={{ fontSize: '11px', color: 'var(--color-warning, #f59e0b)', margin: '6px 0 0', lineHeight: 1.5 }}>
                    ⚠ 請先到「Local LLM Server」頁面設定 llama 路徑與本地模型。
                  </p>
                )}
                {draft.voice_process_mode !== 'none' && (
                  <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '6px 0 0', lineHeight: 1.5 }}>
                    {draft.voice_process_mode === 'format'
                      ? '辨識完成後，llama 會將口語文字潤飾成書面語後再插入。'
                      : '辨識完成後，llama 會分析口語文字中的關鍵主題，並將其替換為 [[wikilink]] 格式後插入。'}
                  </p>
                )}
              </div>

              {/* ── Model Management ───────────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="模型管理" locked={whisperStatus === 'running'} />
              <ModelDownloader
                models={WHISPER_MODELS}
                title="語音辨識模型"
                kind="whisper"
                value={draft.whisper_model_path}
                onChange={(v) => up({ whisper_model_path: v })}
                disabled={whisperStatus === 'running'}
              />
            </>}

            {tab === 'local' && <>
              {/* ── Server Status ──────────────────────────────────────────── */}
              <ServerCard
                name="LLaMA Server（本地 AI）"
                status={llamaStatus}
                busy={llamaBusy}
                onStart={async () => {
                  setLlamaBusy(true)
                  try {
                    await invoke('start_llama_server')
                  } catch (e: any) {
                    console.error('[start_llama_server]', e)
                    const msg = typeof e === 'string' ? e : e?.AI ?? e?.message ?? JSON.stringify(e)
                    toast.error(msg)
                  } finally {
                    setLlamaBusy(false)
                  }
                }}
                onStop={async () => {
                  setLlamaBusy(true)
                  await invoke('stop_llama_server').catch(() => {})
                  setLlamaStatus('stopped')
                  setLlamaBusy(false)
                }}
                onRestart={async () => {
                  setLlamaBusy(true)
                  try {
                    await invoke('restart_llama_server')
                  } catch (e: any) {
                    console.error('[restart_llama_server]', e)
                    const msg = typeof e === 'string' ? e : e?.AI ?? e?.message ?? JSON.stringify(e)
                    toast.error(msg)
                  } finally {
                    setLlamaBusy(false)
                  }
                }}
              />

              {/* ── Server Configuration ───────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="伺服器設定" locked={llamaStatus === 'running'} />
              <div style={fieldStyle}>
                <label style={labelStyle}>執行檔路徑</label>
                <PathPicker value={draft.llama_cli_path} onChange={(v) => up({ llama_cli_path: v })} disabled={llamaStatus === 'running'} />
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                  請指向 <code>llama-server</code> 二進位檔（非 llama-cli）
                </p>
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>自動下載</label>
                <button
                  onClick={() => {
                    setLlamaBinaryDownloading(true)
                    invoke('download_llama_server').catch((e: any) => {
                      setLlamaBinaryDownloading(false)
                      toast.error('下載失敗：' + (e?.Import ?? String(e)))
                    })
                  }}
                  disabled={llamaBinaryDownloading || llamaStatus === 'running'}
                  style={{ ...inputStyle, cursor: llamaBinaryDownloading || llamaStatus === 'running' ? 'not-allowed' : 'pointer', opacity: llamaBinaryDownloading || llamaStatus === 'running' ? 0.6 : 1 }}
                >
                  {llamaBinaryDownloading ? '下載中…' : llamaBinaryInstalled ? '重新安裝最新版' : '自動安裝最新版'}
                </button>
                {llamaBinaryDownloading && (
                  <div style={{ marginTop: '8px' }}>
                    <div style={{ height: '4px', borderRadius: '2px', background: 'var(--color-bg-overlay)', overflow: 'hidden' }}>
                      <div style={{
                        height: '100%',
                        width: llamaBinaryTotalBytes > 0 ? `${Math.round(llamaBinaryDownloadedBytes / llamaBinaryTotalBytes * 100)}%` : '100%',
                        background: 'var(--color-accent)',
                        borderRadius: '2px',
                        transition: llamaBinaryTotalBytes > 0 ? 'width 0.3s ease' : undefined,
                        animation: llamaBinaryTotalBytes === 0 ? 'pulse 1.5s ease-in-out infinite' : undefined,
                      }} />
                    </div>
                    <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '4px' }}>
                      <span>
                        {fmtBytes(llamaBinaryDownloadedBytes)}
                        {llamaBinaryTotalBytes > 0 ? ` / ${fmtBytes(llamaBinaryTotalBytes)}` : ''}
                      </span>
                      <span>
                        {llamaBinaryTotalBytes > 0 ? `${Math.round(llamaBinaryDownloadedBytes / llamaBinaryTotalBytes * 100)}%` : '連線中…'}
                        {llamaBinarySpeedBps > 0 ? ` · ${fmtSpeed(llamaBinarySpeedBps)}` : ''}
                      </span>
                    </div>
                  </div>
                )}
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                  從 ggerganov/llama.cpp 官方 Releases 下載預建版本（macOS ARM / Windows AVX2）。下載後自動填入路徑。
                </p>
              </div>

              {/* ── AI Features ────────────────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="AI 功能" />
              <ToggleRow label="啟用智慧摘要" value={draft.ai_enable_summary} onChange={(v) => up({ ai_enable_summary: v })} />
              <ToggleRow label="啟用主題分析" value={draft.ai_enable_topics} onChange={(v) => up({ ai_enable_topics: v })} />
              <ToggleRow label="啟用圖片辨識" value={draft.ai_enable_vision} onChange={(v) => up({ ai_enable_vision: v })} />
              <ToggleRow label="啟用 Chat 功能" value={draft.enable_chat} onChange={(v) => up({ enable_chat: v })} />
              {draft.enable_chat && (
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '-8px 0 16px 0', lineHeight: 1.5 }}>
                  開啟後右側面板會新增 Chat 分頁，可與本地 llama LLM 進行對話。需先設定上方的 llama CLI 路徑。
                </p>
              )}
              <ToggleRow label="Chat 自動帶入當前筆記" value={draft.chat_auto_include_note} onChange={(v) => up({ chat_auto_include_note: v })} />
              {draft.chat_auto_include_note && (
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '-8px 0 16px 0', lineHeight: 1.5 }}>
                  開啟筆記時，Chat 會自動將目前編輯中的筆記內容注入為 system context，讓 LLM 可直接針對筆記回答。
                </p>
              )}

              {/* ── Chat & Memory ──────────────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="Chat 與記憶" />
              <ToggleRow label="自動記憶整理" value={draft.enable_auto_memory} onChange={(v) => up({ enable_auto_memory: v })} />
              {draft.enable_auto_memory && (
                <>
                  <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '-8px 0 4px 0', lineHeight: 1.5 }}>
                    對話達到閾值時自動將原始訊息存為記憶筆記（memories/ai_memory_*.md），並提供 query_memory 工具查詢。
                  </p>
                  <div style={{ display: 'flex', alignItems: 'center', gap: '12px', marginBottom: '16px' }}>
                    <label style={{ fontSize: '13px', color: 'var(--color-text-secondary)', whiteSpace: 'nowrap' }}>訊息閾值</label>
                    <input
                      type="number" min={5} max={200}
                      value={draft.memory_threshold}
                      onChange={(e) => up({ memory_threshold: Math.max(5, Math.min(200, Number(e.target.value))) })}
                      style={{
                        width: '72px', padding: '4px 8px',
                        background: 'var(--color-bg-base)', border: '1px solid var(--color-border)',
                        borderRadius: '4px', color: 'var(--color-text-primary)', fontSize: '13px',
                      }}
                    />
                    <span style={{ fontSize: '12px', color: 'var(--color-text-muted)' }}>則訊息後壓縮</span>
                  </div>
                </>
              )}
              {/* 寫入確認模式 */}
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: '16px' }}>
                <div>
                  <span style={{ fontSize: '13px', color: 'var(--color-text-primary)' }}>Vault 寫入確認</span>
                  <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '3px 0 0', lineHeight: 1.4 }}>
                    LLM 呼叫寫入工具（新增/更新筆記、新增資料夾）時的確認方式
                  </p>
                </div>
                <div style={{ display: 'flex', gap: '4px', flexShrink: 0, marginLeft: '12px' }}>
                  {(['always', 'once', 'never'] as const).map((mode) => (
                    <button
                      key={mode}
                      onClick={() => up({ write_confirm_mode: mode })}
                      style={{
                        padding: '3px 10px', borderRadius: '4px', fontSize: '12px',
                        border: '1px solid var(--color-border)',
                        background: draft.write_confirm_mode === mode ? 'var(--color-accent)' : 'transparent',
                        color: draft.write_confirm_mode === mode ? '#fff' : 'var(--color-text-muted)',
                        cursor: 'pointer',
                      }}
                    >
                      {mode === 'always' ? '每次' : mode === 'once' ? '本次' : '關閉'}
                    </button>
                  ))}
                </div>
              </div>

              {/* 查詢規則 */}
              <div style={{ marginBottom: '16px' }}>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: '10px' }}>
                  <span style={{ fontSize: '13px', fontWeight: 500, color: 'var(--color-text-primary)' }}>查詢規則</span>
                  {memoryRulesLoading && (
                    <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>載入中…</span>
                  )}
                </div>
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '0 0 12px', lineHeight: 1.5 }}>
                  由 AI 自動學習並寫入的時間詞與停用詞規則，用於加速記憶查詢。可刪除不需要的規則。
                </p>
                {!memoryRulesLoading && memoryRules.length === 0 ? (
                  <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', fontStyle: 'italic' }}>尚無自訂規則</p>
                ) : (
                  <div style={{ border: '1px solid var(--color-border)', borderRadius: '6px', overflow: 'hidden' }}>
                    <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: '12px' }}>
                      <thead>
                        <tr style={{ background: 'var(--color-bg-elevated)' }}>
                          {['類型', '觸發詞', '值', '建立日期', ''].map((h) => (
                            <th key={h} style={{
                              textAlign: 'left', padding: '6px 10px',
                              color: 'var(--color-text-secondary)', fontWeight: 500,
                              borderBottom: '1px solid var(--color-border)',
                            }}>{h}</th>
                          ))}
                        </tr>
                      </thead>
                      <tbody>
                        {memoryRules.map((rule, i) => (
                          <tr key={rule.id} style={{ background: i % 2 === 0 ? 'transparent' : 'var(--color-bg-elevated)' }}>
                            <td style={{ padding: '6px 10px', color: 'var(--color-text-secondary)' }}>
                              {patternTypeLabel(rule.pattern_type)}
                            </td>
                            <td style={{ padding: '6px 10px', color: 'var(--color-text-primary)', fontFamily: 'monospace' }}>
                              {rule.pattern}
                            </td>
                            <td style={{ padding: '6px 10px', color: 'var(--color-text-secondary)', fontFamily: 'monospace' }}>
                              {rule.value || '—'}
                            </td>
                            <td style={{ padding: '6px 10px', color: 'var(--color-text-muted)', whiteSpace: 'nowrap' }}>
                              {new Date(rule.created_at).toLocaleDateString('zh-TW')}
                            </td>
                            <td style={{ padding: '6px 8px', textAlign: 'right' }}>
                              <button
                                onClick={() => handleDeleteMemoryRule(rule.id)}
                                style={{
                                  padding: '2px 8px', borderRadius: '4px', fontSize: '11px',
                                  background: 'transparent', border: '1px solid var(--color-border)',
                                  color: 'var(--color-text-muted)', cursor: 'pointer',
                                }}
                                onMouseEnter={(e) => {
                                  e.currentTarget.style.borderColor = '#e06c75'
                                  e.currentTarget.style.color = '#e06c75'
                                }}
                                onMouseLeave={(e) => {
                                  e.currentTarget.style.borderColor = 'var(--color-border)'
                                  e.currentTarget.style.color = 'var(--color-text-muted)'
                                }}
                              >刪除</button>
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </div>

              {/* ── Model Management ───────────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="模型管理" locked={llamaStatus === 'running'} />
              <ModelDownloader
                models={LLM_MODELS}
                title="本地語言模型"
                kind="llm"
                value={draft.llm_model_path}
                onChange={(v) => up({ llm_model_path: v })}
                disabled={llamaStatus === 'running'}
              />
            </>}

            {tab === 'advanced' && <>
              <div style={fieldStyle}>
                <label style={labelStyle}>匯入最大深度</label>
                <input type="number" min={1} max={10}
                  value={draft.import_max_depth}
                  onChange={(e) => up({ import_max_depth: Number(e.target.value) })}
                  style={numInputStyle} />
              </div>
              <div style={fieldStyle}>
                <label style={labelStyle}>匯入最大頁數</label>
                <input type="number" min={1} max={200}
                  value={draft.import_max_pages}
                  onChange={(e) => up({ import_max_pages: Number(e.target.value) })}
                  style={numInputStyle} />
              </div>
              <div style={{ borderTop: '1px solid var(--color-border)', paddingTop: '16px', marginTop: '8px' }}>
                <ToggleRow
                  label="Debug 模式"
                  value={draft.debug_mode}
                  onChange={(v) => up({ debug_mode: v })}
                />
                {draft.debug_mode && (
                  <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '-8px 0 16px 0', lineHeight: 1.5 }}>
                    開啟後右側面板會新增 Debug 分頁，顯示語音錄音的詳細事件日誌。
                  </p>
                )}
              </div>
            </>}

            {tab === 'raw' && (
              <div>
                <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', marginBottom: '12px' }}>
                  目前 draft 設定（未儲存的變更）— 僅供檢閱，不可直接編輯。
                </p>
                <pre style={{
                  background: 'var(--color-bg-base)', border: '1px solid var(--color-border)',
                  borderRadius: '6px', padding: '14px 16px',
                  fontSize: '11.5px', color: 'var(--color-text-primary)',
                  fontFamily: "'JetBrains Mono', monospace",
                  overflowX: 'auto', whiteSpace: 'pre',
                  margin: 0, lineHeight: 1.6,
                }}>
                  {JSON.stringify({ ...draft, _note: 'API Key 儲存於系統 Keychain，不顯示於此' }, null, 2)}
                </pre>
              </div>
            )}

          </div>
        </div>

        {/* Footer */}
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '12px 20px', borderTop: '1px solid var(--color-border)', flexShrink: 0 }}>
          <button
            onClick={handleReset}
            style={{ padding: '7px 16px', borderRadius: '6px', background: 'transparent', border: '1px solid var(--color-border)', color: 'var(--color-text-secondary)', fontSize: '13px' }}
            onMouseEnter={(e) => (e.currentTarget.style.borderColor = 'var(--color-text-muted)')}
            onMouseLeave={(e) => (e.currentTarget.style.borderColor = 'var(--color-border)')}
          >還原預設</button>
          <div style={{ display: 'flex', gap: '8px' }}>
            <button
              onClick={onClose}
              style={{ padding: '7px 16px', borderRadius: '6px', background: 'transparent', border: '1px solid var(--color-border)', color: 'var(--color-text-secondary)', fontSize: '13px' }}
              onMouseEnter={(e) => (e.currentTarget.style.borderColor = 'var(--color-text-muted)')}
              onMouseLeave={(e) => (e.currentTarget.style.borderColor = 'var(--color-border)')}
            >取消</button>
            <button
              onClick={handleSave}
              disabled={isSaving}
              style={{ padding: '7px 20px', borderRadius: '6px', background: 'var(--color-accent)', color: '#fff', fontSize: '13px', fontWeight: 500, opacity: isSaving ? 0.7 : 1 }}
            >{isSaving ? '儲存中…' : '儲存設定'}</button>
          </div>
        </div>
      </div>
    </div>
  )
}
