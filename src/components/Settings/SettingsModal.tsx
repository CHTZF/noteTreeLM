import { useState, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { open, ask, message } from '@tauri-apps/plugin-dialog'
import { useTranslation } from 'react-i18next'
import { useSettingsStore, SYSTEM_KEYS, SystemSettings } from '../../stores/settingsStore'
import { useAuthStore } from '../../stores/authStore'
import { Settings, DEFAULT_SETTINGS } from '../../types/settings'
import ModelDownloader, { WHISPER_MODELS, LLM_MODELS, EMBEDDING_MODELS } from './ModelDownloader'
import { toast } from '../common/Toast'
import { SUPPORTED_LANGUAGES } from '../../i18n'

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
  onClose?: () => void
  inline?: boolean
  mode?: 'system' | 'personal'
}

interface MemoryRuleEntry {
  id: number
  pattern_type: string
  pattern: string
  value: string
  created_at: number
}

type Tab = 'account' | 'general' | 'ai' | 'voice' | 'local' | 'advanced' | 'raw' | 'memory'
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

interface RepairLog {
  filename: string
  timestamp: string
  reason: string
  result: string
  tables_restored: Record<string, number>
}

export default function SettingsModal({ onClose, inline, mode = 'personal' }: SettingsModalProps) {
  const { t, i18n } = useTranslation()
  const { settings, savePersonal, saveSystem, loadSystem, systemSettings, getApiKey, setApiKey } = useSettingsStore()
  const { session } = useAuthStore()

  const [tab, setTab] = useState<Tab>(mode === 'system' ? 'ai' : 'general')
  const [repairLogs, setRepairLogs] = useState<RepairLog[]>([])
  const [expandedLog, setExpandedLog] = useState<string | null>(null)
  const [draft, setDraft] = useState<Settings>(() =>
    mode === 'system'
      ? { ...DEFAULT_SETTINGS, ...(systemSettings ?? {}) }
      : { ...settings }
  )
  const [apiKey, setApiKeyLocal] = useState('')
  const [apiKeySaved, setApiKeySaved] = useState(false)
  const [braveApiKey, setBraveApiKey] = useState('')
  const [braveApiKeySaved, setBraveApiKeySaved] = useState(false)
  const [braveUsage, setBraveUsage] = useState<{ used: number; limit: number; reset_label: string } | null>(null)
  const [isSaving, setIsSaving] = useState(false)
  const [recordingKey, setRecordingKey] = useState<string | null>(null)

  const isMac = navigator.platform.startsWith('Mac')
  const displayHotkey = (combo: string) => {
    if (!combo) return '未設定'
    return combo.split('+').map(p => {
      if (p === 'mod') return isMac ? '⌘' : 'Ctrl'
      if (p === 'ctrl') return '⌃'
      if (p === 'alt') return isMac ? '⌥' : 'Alt'
      if (p === 'shift') return '⇧'
      return p.length === 1 ? p.toUpperCase() : p
    }).join(isMac ? '' : '+')
  }
  const captureKey = (e: React.KeyboardEvent, field: keyof Settings) => {
    e.preventDefault()
    e.stopPropagation()
    if (['Meta', 'Control', 'Alt', 'Shift'].includes(e.key)) return
    const parts: string[] = []
    if (e.metaKey || e.ctrlKey) parts.push('mod')
    if (e.shiftKey) parts.push('shift')
    if (e.altKey) parts.push('alt')
    parts.push(e.key.toLowerCase())
    up({ [field]: parts.join('+') } as any)
    setRecordingKey(null)
  }

  const [whisperStatus, setWhisperStatus] = useState<ServerStatus>('unknown')
  const [llamaStatus, setLlamaStatus] = useState<ServerStatus>('unknown')
  const [embeddingStatus, setEmbeddingStatus] = useState<ServerStatus>('unknown')
  const [whisperBusy, setWhisperBusy] = useState(false)
  const [llamaBusy, setLlamaBusy] = useState(false)
  const [embeddingBusy, setEmbeddingBusy] = useState(false)
  // Whisper 推論速度測試（毫秒/秒音頻，即 RTF × 1000）
  const [whisperBenchmarkMs, setWhisperBenchmarkMs] = useState<number | null>(null)
  const [whisperBenchmarking, setWhisperBenchmarking] = useState(false)
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

  const [coremlInstalled, setCoremlInstalled] = useState(false)
  const [coremlDownloading, setCoremlDownloading] = useState(false)
  const [coremlDownloadedBytes, setCoremlDownloadedBytes] = useState(0)
  const [coremlTotalBytes, setCoremlTotalBytes] = useState(0)
  const [coremlSpeedBps, setCoremlSpeedBps] = useState(0)

  const [memoryRules, setMemoryRules] = useState<MemoryRuleEntry[]>([])
  const [memoryRulesLoading, setMemoryRulesLoading] = useState(false)
  const [memoryAgentRunning, setMemoryAgentRunning] = useState(false)
  const [memoryAgentMsg, setMemoryAgentMsg] = useState('')
  const [newRuleType, setNewRuleType] = useState<string>('temporal_exact_days')
  const [newRulePattern, setNewRulePattern] = useState('')
  const [newRuleValue, setNewRuleValue] = useState('')

  // Account tab state
  const [currentPassword, setCurrentPassword] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const [pwSaving, setPwSaving] = useState(false)

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

  // 系統設定模式：掛載時拉取 global settings，並在資料回來後同步 draft
  useEffect(() => {
    if (mode === 'system') loadSystem()
  }, [mode])

  useEffect(() => {
    if (mode === 'system' && systemSettings)
      setDraft(prev => ({ ...prev, ...systemSettings }))
  }, [systemSettings])

  useEffect(() => {
    if (draft.ai_provider)
      getApiKey(draft.ai_provider).then((k) => setApiKeyLocal(k || ''))
    else
      setApiKeyLocal('')
  }, [draft.ai_provider])

  useEffect(() => {
    getApiKey('brave_search').then((k) => setBraveApiKey(k || ''))
    invoke<{ used: number; limit: number; reset_label: string }>('get_brave_search_usage')
      .then(setBraveUsage).catch(() => {})
  }, [])

  useEffect(() => {
    if (tab !== 'voice' && tab !== 'local') return
    const refresh = async () => {
      const ws = await invoke<string>('get_whisper_server_status').catch(() => 'stopped')
      setWhisperStatus(ws as ServerStatus)
      const ls = await invoke<string>('get_llama_server_status').catch(() => 'stopped')
      setLlamaStatus(ls as ServerStatus)
      const es = await invoke<string>('get_embedding_server_status').catch(() => 'stopped')
      setEmbeddingStatus(es as ServerStatus)
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

  // 模型更換時清除舊測速結果
  useEffect(() => { setWhisperBenchmarkMs(null) }, [draft.whisper_model_path])

  // Whisper 推論速度自動測速：Voice 頁 + 伺服器已啟動 + 尚未測過
  // 發送 1 秒 440Hz 正弦波，測 round-trip 時間（包含 IPC + 推論）
  useEffect(() => {
    if (tab !== 'voice') return
    if (whisperStatus !== 'running') return
    if (whisperBenchmarkMs !== null) return
    if (whisperBenchmarking) return
    if (!draft.whisper_model_path) return
    setWhisperBenchmarking(true)
    const sampleRate = 16000
    const pcmData = Array.from({ length: sampleRate }, (_, i) =>
      Math.sin(2 * Math.PI * 440 * i / sampleRate) * 0.3,
    )
    const t0 = Date.now()
    invoke<{ text: string }>('transcribe_audio', { pcmData, sampleRate })
      .then(() => { setWhisperBenchmarkMs(Date.now() - t0) })
      .catch(() => {})
      .finally(() => setWhisperBenchmarking(false))
  }, [tab, whisperStatus, whisperBenchmarkMs, whisperBenchmarking, draft.whisper_model_path])

  // 檢查 CoreML 模型套件是否已安裝
  useEffect(() => {
    if (tab !== 'voice' || !draft.whisper_model_path) return
    invoke<string | null>('get_coreml_model_path', { modelPath: draft.whisper_model_path })
      .then(p => setCoremlInstalled(!!p))
  }, [tab, draft.whisper_model_path])

  // 監聽 CoreML 下載進度
  useEffect(() => {
    let unlisten: (() => void) | null = null
    let cancelled = false
    listen<any>('model-download-progress', (e) => {
      if (e.payload.model_id !== '__coreml__') return
      const p = e.payload
      if (p.status === 'downloading') {
        setCoremlDownloadedBytes(p.downloaded_bytes ?? 0)
        setCoremlTotalBytes(p.total_bytes ?? 0)
        setCoremlSpeedBps(p.speed_bps ?? 0)
      } else if (p.status === 'completed') {
        setCoremlDownloading(false)
        setCoremlDownloadedBytes(0)
        setCoremlTotalBytes(0)
        setCoremlSpeedBps(0)
        setCoremlInstalled(true)
        toast.success('CoreML 模型下載完成，下次啟動 whisper-server 時自動生效')
      } else if (p.status === 'error') {
        setCoremlDownloading(false)
        setCoremlDownloadedBytes(0)
        setCoremlTotalBytes(0)
        setCoremlSpeedBps(0)
        toast.error('CoreML 下載失敗：' + p.error)
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
    if (tab !== 'memory') return
    setMemoryRulesLoading(true)
    invoke<MemoryRuleEntry[]>('get_memory_rules')
      .then(setMemoryRules)
      .catch(() => setMemoryRules([]))
      .finally(() => setMemoryRulesLoading(false))
  }, [tab])

  useEffect(() => {
    if (tab !== 'advanced') return
    invoke<RepairLog[]>('list_repair_logs')
      .then(setRepairLogs)
      .catch(() => setRepairLogs([]))
  }, [tab])

  const handleDeleteMemoryRule = async (id: number) => {
    await invoke('delete_memory_rule', { id }).catch(() => {})
    setMemoryRules((prev) => prev.filter((r) => r.id !== id))
  }

  const handleAddMemoryRule = async () => {
    const pattern = newRulePattern.trim()
    if (!pattern) return
    await invoke('add_memory_rule', { patternType: newRuleType, pattern, value: newRuleValue.trim() }).catch(() => {})
    // 重新載入以取得 server 產生的 id
    const updated = await invoke<MemoryRuleEntry[]>('get_memory_rules').catch(() => memoryRules)
    setMemoryRules(updated)
    setNewRulePattern('')
    setNewRuleValue('')
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

  const handleRepair = async () => {
    const confirmed = await ask(
      '此操作將備份設定、帳號、技能、Agent 定義及對話紀錄，\n然後在重啟時重建資料庫並自動還原。\n\n重啟後搜尋索引需重新執行 reindex。\n\n確定要繼續嗎？',
      { title: '修復資料庫', kind: 'warning', okLabel: '確定修復', cancelLabel: '取消' }
    )
    if (!confirmed) return
    try {
      await invoke('prepare_db_repair')
      await message('備份完成，請重新啟動 App。\n重啟後資料庫將自動修復並還原資料。', {
        title: '修復準備完成', kind: 'info'
      })
    } catch (e: any) {
      await message('修復準備失敗：' + (typeof e === 'string' ? e : JSON.stringify(e)), {
        title: '錯誤', kind: 'error'
      })
    }
  }

  const handleSave = async () => {
    setIsSaving(true)
    try {
      if (mode === 'system') {
        const systemDraft = Object.fromEntries(
          SYSTEM_KEYS.map(k => [k, draft[k]])
        ) as unknown as SystemSettings
        await saveSystem(systemDraft)
      } else {
        await savePersonal(draft)
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
      }
      toast.success('設定已儲存')
      if (!inline) onClose?.()
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
      system_current_vault_path: systemSettings?.system_current_vault_path ?? settings.system_current_vault_path,
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

  const handleSaveBraveApiKey = async () => {
    await setApiKey('brave_search', braveApiKey)
    // Persist key_id to DB — usage queries will use DB, never keychain directly
    await invoke('sync_brave_key_id', { key: braveApiKey }).catch(() => {})
    setBraveApiKeySaved(true)
    setTimeout(() => setBraveApiKeySaved(false), 2000)
    invoke<{ used: number; limit: number; reset_label: string }>('get_brave_search_usage')
      .then(setBraveUsage).catch(() => {})
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
          let defaultPath: string | undefined
          if (value) {
            if (isDir) {
              defaultPath = value
            } else {
              const sep = value.includes('\\') ? '\\' : '/'
              const idx = value.lastIndexOf(sep)
              if (idx > 0) defaultPath = value.substring(0, idx)
            }
          }
          const r = await open({ directory: isDir, multiple: false, ...(defaultPath ? { defaultPath } : {}) })
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


  const tabs: [Tab, string][] = mode === 'system'
    ? [['ai', t('settings.tab.ai')], ['voice', t('settings.tab.voice')], ['local', t('settings.tab.local')], ['advanced', t('settings.tab.advanced')], ['raw', t('settings.tab.raw')]]
    : [['account', t('settings.tab.account')], ['general', t('settings.tab.general')], ['advanced', t('settings.tab.advanced')], ['memory', t('settings.tab.memory')], ['raw', t('settings.tab.raw')]]

  const handleChangePassword = async () => {
    if (!newPassword || newPassword !== confirmPassword) {
      toast.error('新密碼與確認密碼不一致')
      return
    }
    setPwSaving(true)
    try {
      await invoke('change_password', { currentPassword, newPassword })
      toast.success('密碼已更新')
      setCurrentPassword('')
      setNewPassword('')
      setConfirmPassword('')
    } catch (e: any) {
      toast.error(e.message || '密碼更新失敗')
    } finally {
      setPwSaving(false)
    }
  }

  const avatarInitials = draft.display_name?.slice(0, 2).toUpperCase() || session?.username?.charAt(0).toUpperCase() || '?'
  const avatarBg = draft.avatar_type === 'image' && draft.avatar_image ? 'transparent' : (draft.avatar_color || '#0a84ff')

  const AVATAR_PRESET_COLORS = [
    '#0a84ff', '#30d158', '#ff9f0a', '#ff453a', '#bf5af2',
    '#64d2ff', '#ff6961', '#ffd60a', '#34c759', '#5e5ce6',
    '#8e8e93', '#636366', '#3a3a3c', '#a2845e', '#e17055',
  ]

  function handleAvatarImagePick() {
    const input = document.createElement('input')
    input.type = 'file'
    input.accept = 'image/*'
    input.onchange = () => {
      const file = input.files?.[0]
      if (!file) return
      const reader = new FileReader()
      reader.onload = () => {
        const dataUrl = reader.result as string
        // Resize to 256px max via canvas
        const img = new Image()
        img.onload = () => {
          const size = 256
          const canvas = document.createElement('canvas')
          const scale = Math.min(1, size / Math.max(img.width, img.height))
          canvas.width = Math.round(img.width * scale)
          canvas.height = Math.round(img.height * scale)
          canvas.getContext('2d')!.drawImage(img, 0, 0, canvas.width, canvas.height)
          up({ avatar_image: canvas.toDataURL('image/jpeg', 0.85), avatar_type: 'image' })
        }
        img.src = dataUrl
      }
      reader.readAsDataURL(file)
    }
    input.click()
  }

  const numInputStyle: React.CSSProperties = {
    ...inputStyle,
    // Reserve space for spin buttons so the right gap matches a <select>'s arrow area
    paddingRight: '4px',
  }

  const modalInnerStyle: React.CSSProperties = inline
    ? { flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden', borderRadius: 0 }
    : { width: 620, height: 540, borderRadius: '12px', background: 'var(--color-bg-surface)', border: '1px solid var(--color-border)', boxShadow: 'var(--shadow-lg)', display: 'flex', flexDirection: 'column', overflow: 'hidden' }

  const innerModal = (
    <div id="settings-modal" style={modalInnerStyle}>
        {/* Header */}
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '14px 20px', borderBottom: '1px solid var(--color-border)', flexShrink: 0 }}>
          <span style={{ fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)' }}>{mode === 'system' ? t('settings.system_title') : t('settings.title')}</span>
          {!inline && onClose && (
            <button onClick={onClose}
              style={{ color: 'var(--color-text-secondary)', fontSize: '20px', lineHeight: 1, width: '28px', height: '28px', display: 'flex', alignItems: 'center', justifyContent: 'center', borderRadius: '4px' }}
              onMouseEnter={(e) => (e.currentTarget.style.color = 'var(--color-text-primary)')}
              onMouseLeave={(e) => (e.currentTarget.style.color = 'var(--color-text-secondary)')}
            >×</button>
          )}
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

            {tab === 'account' && <>
              {/* Avatar */}
              <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', marginBottom: '24px', paddingTop: '8px', gap: '10px' }}>
                {/* Preview */}
                <div style={{
                  width: '72px', height: '72px', borderRadius: '50%', overflow: 'hidden',
                  background: avatarBg, display: 'flex', alignItems: 'center', justifyContent: 'center',
                  fontSize: '28px', color: '#fff', fontWeight: 700, userSelect: 'none', flexShrink: 0,
                }}>
                  {draft.avatar_type === 'image' && draft.avatar_image
                    ? <img src={draft.avatar_image} style={{ width: '100%', height: '100%', objectFit: 'cover' }} />
                    : avatarInitials}
                </div>
                <div style={{ fontSize: '14px', fontWeight: 600, color: 'var(--color-text-primary)' }}>{session?.username ?? ''}</div>

                {/* Mode toggle */}
                <div style={{ display: 'flex', gap: '6px', background: 'var(--color-bg-elevated)', borderRadius: '8px', padding: '3px' }}>
                  {(['initials', 'image'] as const).map(mode => (
                    <button key={mode}
                      onClick={() => up({ avatar_type: mode })}
                      style={{
                        padding: '4px 14px', borderRadius: '6px', border: 'none', cursor: 'pointer',
                        fontSize: '12px', fontWeight: 500,
                        background: draft.avatar_type === mode ? 'var(--color-bg-surface)' : 'transparent',
                        color: draft.avatar_type === mode ? 'var(--color-text-primary)' : 'var(--color-text-muted)',
                        boxShadow: draft.avatar_type === mode ? '0 1px 3px rgba(0,0,0,0.15)' : 'none',
                        transition: 'all 0.15s',
                      }}>
                      {mode === 'initials' ? '縮寫' : '圖片'}
                    </button>
                  ))}
                </div>

                {/* Initials: color picker */}
                {draft.avatar_type === 'initials' && (
                  <div style={{ display: 'flex', flexWrap: 'wrap', gap: '6px', justifyContent: 'center', maxWidth: '200px' }}>
                    {AVATAR_PRESET_COLORS.map(c => (
                      <button key={c} onClick={() => up({ avatar_color: c })}
                        style={{
                          width: '24px', height: '24px', borderRadius: '50%', border: 'none', cursor: 'pointer',
                          background: c, padding: 0,
                          boxShadow: draft.avatar_color === c ? `0 0 0 2px var(--color-bg-surface), 0 0 0 4px ${c}` : 'none',
                          transition: 'box-shadow 0.12s',
                        }} />
                    ))}
                  </div>
                )}

                {/* Image: upload button */}
                {draft.avatar_type === 'image' && (
                  <div style={{ display: 'flex', gap: '8px' }}>
                    <button onClick={handleAvatarImagePick}
                      style={{ padding: '5px 14px', borderRadius: '6px', border: '1px solid var(--color-border)', background: 'var(--color-bg-elevated)', color: 'var(--color-text-primary)', fontSize: '12px', cursor: 'pointer' }}>
                      選擇圖片
                    </button>
                    {draft.avatar_image && (
                      <button onClick={() => up({ avatar_image: '', avatar_type: 'initials' })}
                        style={{ padding: '5px 14px', borderRadius: '6px', border: '1px solid var(--color-border)', background: 'transparent', color: 'var(--color-text-muted)', fontSize: '12px', cursor: 'pointer' }}>
                        移除
                      </button>
                    )}
                  </div>
                )}
              </div>

              <div style={fieldStyle}>
                <label style={labelStyle}>顯示名稱</label>
                <input value={draft.display_name ?? ''} onChange={(e) => up({ display_name: e.target.value })} placeholder={session?.username ?? ''} style={inputStyle} />
              </div>
              {session?.auth_provider !== 'google' && <>
                <div style={{ height: '1px', background: 'var(--color-border)', margin: '4px 0 20px' }} />
                <div style={{ fontSize: '12px', fontWeight: 600, letterSpacing: '0.07em', color: 'var(--color-text-secondary)', textTransform: 'uppercase', marginBottom: '12px' }}>變更密碼</div>
                <div style={fieldStyle}>
                  <label style={labelStyle}>目前密碼</label>
                  <input type="password" value={currentPassword} onChange={(e) => setCurrentPassword(e.target.value)} style={inputStyle} />
                </div>
                <div style={fieldStyle}>
                  <label style={labelStyle}>新密碼</label>
                  <input type="password" value={newPassword} onChange={(e) => setNewPassword(e.target.value)} style={inputStyle} />
                </div>
                <div style={fieldStyle}>
                  <label style={labelStyle}>確認新密碼</label>
                  <input
                    type="password"
                    value={confirmPassword}
                    onChange={(e) => setConfirmPassword(e.target.value)}
                    style={{
                      ...inputStyle,
                      ...(confirmPassword && newPassword !== confirmPassword
                        ? { borderColor: 'var(--color-danger)', outline: 'none' }
                        : confirmPassword && newPassword === confirmPassword
                        ? { borderColor: 'var(--color-success)' }
                        : {}),
                    }}
                  />
                  {confirmPassword && newPassword !== confirmPassword && (
                    <div style={{ fontSize: '11px', color: 'var(--color-danger)', marginTop: '4px' }}>密碼不一致</div>
                  )}
                </div>
                <button
                  onClick={handleChangePassword}
                  disabled={pwSaving || (!!confirmPassword && newPassword !== confirmPassword)}
                  style={{ padding: '7px 20px', borderRadius: '6px', background: 'var(--color-accent)', color: '#fff', fontSize: '13px', fontWeight: 500, opacity: (pwSaving || (!!confirmPassword && newPassword !== confirmPassword)) ? 0.5 : 1, cursor: (pwSaving || (!!confirmPassword && newPassword !== confirmPassword)) ? 'not-allowed' : 'pointer' }}
                >{pwSaving ? '儲存中…' : '更新密碼'}</button>
              </>}
            </>}

            {tab === 'general' && <>
              <div style={fieldStyle}>
                <label style={labelStyle}>{t('settings.general.language_label')}</label>
                <select
                  value={i18n.language}
                  onChange={(e) => i18n.changeLanguage(e.target.value)}
                  style={inputStyle}
                >
                  {SUPPORTED_LANGUAGES.map(lang => (
                    <option key={lang.code} value={lang.code}>{lang.label}</option>
                  ))}
                </select>
              </div>
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
                  <option value="'Noto Sans TC', sans-serif">Noto Sans TC（繁體中文）</option>
                  <option value="'Noto Serif TC', serif">Noto Serif TC（繁體中文 襯線）</option>
                  <option value="'LXGW WenKai TC', cursive">霞鶩文楷 TC（繁體中文 手寫風）</option>
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
                  <option value="'Noto Sans TC', sans-serif">Noto Sans TC（繁體中文）</option>
                  <option value="'Noto Serif TC', serif">Noto Serif TC（繁體中文 襯線）</option>
                  <option value="'LXGW WenKai TC', cursive">霞鶩文楷 TC（繁體中文 手寫風）</option>
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

              {/* ── Chat 功能 ────────────────────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="Chat 功能" />
              <ToggleRow label="啟用 Chat 功能" value={draft.enable_chat} onChange={(v) => up({ enable_chat: v })} />
              {draft.enable_chat && (
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '-8px 0 16px 0', lineHeight: 1.5 }}>
                  開啟後右側面板會新增 Chat 分頁，可與本地 llama LLM 進行對話。需先至系統設定設定 llama 執行檔路徑。
                </p>
              )}
              <ToggleRow label="Chat 自動帶入當前筆記" value={draft.chat_auto_include_note} onChange={(v) => up({ chat_auto_include_note: v })} />
              {draft.chat_auto_include_note && (
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '-8px 0 16px 0', lineHeight: 1.5 }}>
                  開啟筆記時，Chat 會自動將目前編輯中的筆記內容注入為 system context，讓 LLM 可直接針對筆記回答。
                </p>
              )}
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: '16px' }}>
                <div>
                  <span style={{ fontSize: '13px', color: 'var(--color-text-primary)' }}>Vault 寫入確認</span>
                  <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '3px 0 0', lineHeight: 1.4 }}>
                    LLM 呼叫寫入工具（新增/更新筆記、新增資料夾）時的確認方式
                  </p>
                </div>
                <div style={{ display: 'flex', gap: '4px', flexShrink: 0, marginLeft: '12px' }}>
                  {(['always', 'once', 'never'] as const).map((m) => (
                    <button
                      key={m}
                      onClick={() => up({ write_confirm_mode: m })}
                      style={{
                        padding: '3px 10px', borderRadius: '4px', fontSize: '12px',
                        border: '1px solid var(--color-border)',
                        background: draft.write_confirm_mode === m ? 'var(--color-accent)' : 'transparent',
                        color: draft.write_confirm_mode === m ? '#fff' : 'var(--color-text-muted)',
                        cursor: 'pointer',
                      }}
                    >
                      {m === 'always' ? '每次' : m === 'once' ? '本次' : '關閉'}
                    </button>
                  ))}
                </div>
              </div>

              <div style={{ marginBottom: '16px' }}>
                <label style={{ fontSize: '13px', color: 'var(--color-text-primary)', display: 'block', marginBottom: '4px' }}>
                  Brave Search API Key
                </label>
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '0 0 6px', lineHeight: 1.4 }}>
                  Chat Agent 呼叫 web_search 工具時使用。至 <a href="https://api.search.brave.com" target="_blank" rel="noreferrer" style={{ color: 'var(--color-accent)' }}>api.search.brave.com</a> 申請免費 API Key（1,000 次/月）。
                </p>
                <div style={{ display: 'flex', gap: '8px' }}>
                  <input
                    type="password"
                    value={braveApiKey}
                    onChange={(e) => setBraveApiKey(e.target.value)}
                    placeholder="BSA…"
                    style={{ ...inputStyle, flex: 1 }}
                  />
                  <button
                    onClick={handleSaveBraveApiKey}
                    style={{
                      height: '32px', padding: '0 14px', borderRadius: '6px',
                      background: braveApiKeySaved ? 'var(--color-success)' : 'var(--color-accent)',
                      color: '#fff', fontSize: '13px', flexShrink: 0, transition: 'background 0.2s', cursor: 'pointer',
                    }}
                  >{braveApiKeySaved ? '已儲存 ✓' : '儲存 Key'}</button>
                </div>
                {braveUsage && (() => {
                  const pct = Math.min(braveUsage.used / braveUsage.limit, 1)
                  const isExhausted = braveUsage.used >= braveUsage.limit
                  const barColor = isExhausted ? 'var(--color-error, #ff453a)' : pct >= 0.8 ? 'var(--color-warning, #ff9f0a)' : 'var(--color-accent)'
                  return (
                    <div style={{ marginTop: '10px' }}>
                      <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: '4px' }}>
                        <span style={{ fontSize: '11px', color: isExhausted ? 'var(--color-error, #ff453a)' : 'var(--color-text-muted)' }}>
                          {isExhausted
                            ? `已達每月搜尋上限，${braveUsage.reset_label}重置`
                            : `本月已使用 ${braveUsage.used} / ${braveUsage.limit} 次`}
                        </span>
                        <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>
                          {Math.round(pct * 100)}%
                        </span>
                      </div>
                      <div style={{ height: '4px', borderRadius: '2px', background: 'var(--color-border)', overflow: 'hidden' }}>
                        <div style={{ height: '100%', width: `${pct * 100}%`, background: barColor, borderRadius: '2px', transition: 'width 0.3s' }} />
                      </div>
                    </div>
                  )
                })()}
              </div>

              {/* ── 快捷鍵 ────────────────────────────────────────────────── */}
              <SectionDivider />
              <SectionHeader label="快捷鍵" />
              {([
                ['hotkey_toggle_view', '切換即時／預覽'],
                ['hotkey_save',        '儲存'],
                ['hotkey_bold',        '粗體'],
                ['hotkey_italic',      '斜體'],
              ] as [keyof Settings, string][]).map(([field, label]) => {
                const isRecording = recordingKey === field
                return (
                  <div key={field} style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: '12px' }}>
                    <span style={{ fontSize: '13px', color: 'var(--color-text-primary)' }}>{label}</span>
                    <div style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
                      <button
                        onKeyDown={isRecording ? (e) => captureKey(e, field) : undefined}
                        onBlur={() => { if (isRecording) setRecordingKey(null) }}
                        onClick={() => setRecordingKey(isRecording ? null : field)}
                        style={{
                          minWidth: '90px', padding: '3px 10px', borderRadius: '5px', fontSize: '13px',
                          border: `1px solid ${isRecording ? 'var(--color-accent)' : 'var(--color-border)'}`,
                          background: isRecording ? 'var(--color-accent-dim, rgba(10,132,255,0.08))' : 'var(--color-bg-base)',
                          color: isRecording ? 'var(--color-accent)' : 'var(--color-text-primary)',
                          cursor: 'pointer', textAlign: 'center' as const, fontFamily: 'monospace',
                          outline: 'none',
                        }}
                        autoFocus={isRecording}
                      >
                        {isRecording ? '按下快捷鍵…' : displayHotkey(String(draft[field] ?? ''))}
                      </button>
                      {draft[field] && (
                        <button onClick={() => up({ [field]: '' } as any)}
                          style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--color-text-muted)', fontSize: '14px', lineHeight: 1, padding: '2px 4px' }}
                          title="清除">×</button>
                      )}
                    </div>
                  </div>
                )
              })}
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
                  <option value="zh-TW">繁體中文</option>
                  <option value="zh-CN">简体中文</option>
                  <option value="en">English</option>
                  <option value="ja">日本語</option>
                  <option value="ko">한국어</option>
                  <option value="fr">Français</option>
                  <option value="de">Deutsch</option>
                </select>
              </div>
              <ToggleRow
                label="錄音時顯示即時預覽（定期送 Whisper 預測）"
                value={draft.voice_preview_enabled ?? true}
                onChange={(v) => up({ voice_preview_enabled: v })}
              />
              <div style={fieldStyle}>
                <label style={labelStyle}>語音處理長度</label>
                <select
                  value={draft.voice_preview_interval ?? 5000}
                  onChange={(e) => up({ voice_preview_interval: Number(e.target.value) })}
                  style={inputStyle}
                  disabled={!(draft.voice_preview_enabled ?? true)}
                >
                  {[1000, 1500, 2000, 2500, 3000, 3500, 4000, 4500, 5000].map((ms) => (
                    <option key={ms} value={ms}>{`${ms / 1000} 秒`}</option>
                  ))}
                </select>
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                  每隔此長度的音頻送一次 Whisper 預覽辨識。
                </p>
              </div>
              {/* 語音響應時間：唯讀顯示，根據選取長度 + 實測 Whisper 速度計算 */}
              {(draft.voice_preview_enabled ?? true) && (
                <div style={fieldStyle}>
                  <label style={labelStyle}>語音響應時間</label>
                  {whisperBenchmarking && (
                    <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', margin: 0, lineHeight: 1.5 }}>
                      ⏱ 測試 Whisper 推論速度中…
                    </p>
                  )}
                  {!whisperBenchmarking && whisperBenchmarkMs !== null && (() => {
                    const intervalMs  = draft.voice_preview_interval ?? 5000
                    const inferMs     = Math.round(whisperBenchmarkMs / 1000 * intervalMs)  // RTF × 段長
                    const totalMs     = intervalMs + inferMs
                    const rtf         = whisperBenchmarkMs / 1000
                    const noteColor   = rtf < 0.3
                      ? 'var(--color-success, #10b981)'
                      : rtf < 0.7
                        ? 'var(--color-warning, #f59e0b)'
                        : 'var(--color-error, #ef4444)'
                    return (
                      <p style={{ fontSize: '12px', color: noteColor, margin: 0, lineHeight: 1.8 }}>
                        約 {(totalMs / 1000).toFixed(1)} 秒
                        <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginLeft: 6 }}>
                          （處理長度 {intervalMs / 1000}s + Whisper 推論 ~{inferMs}ms）
                        </span>
                      </p>
                    )
                  })()}
                  {!whisperBenchmarking && whisperBenchmarkMs === null && (
                    <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: 0, lineHeight: 1.5 }}>
                      {whisperStatus === 'running'
                        ? '測速中，請稍候…'
                        : '啟動 Whisper 伺服器後自動測速並顯示估算值'}
                    </p>
                  )}
                </div>
              )}
              <ToggleRow
                label="RNNoise 雜訊抑制（關閉則使用瀏覽器內建抑制）"
                value={draft.voice_noise_suppression ?? true}
                onChange={(v) => up({ voice_noise_suppression: v })}
              />
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

              {/* ── CoreML Acceleration ─────────────────────────────────────── */}
              {draft.whisper_model_path && (
                <>
                  <SectionDivider />
                  <SectionHeader label="CoreML 加速（Apple Neural Engine）" />
                  <div style={fieldStyle}>
                    <label style={labelStyle}>ANE 加速套件</label>
                    {coremlInstalled ? (
                      <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                        <span style={{ fontSize: '13px', color: 'var(--color-success, #22c55e)' }}>✓ 已安裝</span>
                        <button
                          onClick={() => {
                            setCoremlDownloading(true)
                            invoke('download_coreml_model', { modelPath: draft.whisper_model_path }).catch((e: any) => {
                              setCoremlDownloading(false)
                              toast.error('下載失敗：' + (e?.Import ?? String(e)))
                            })
                          }}
                          disabled={coremlDownloading || whisperStatus === 'running'}
                          style={{ fontSize: '12px', padding: '2px 10px', borderRadius: '5px', background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', color: 'var(--color-text-secondary)', cursor: 'pointer' }}
                        >
                          {coremlDownloading ? '更新中…' : '重新下載'}
                        </button>
                      </div>
                    ) : (
                      <button
                        onClick={() => {
                          setCoremlDownloading(true)
                          invoke('download_coreml_model', { modelPath: draft.whisper_model_path }).catch((e: any) => {
                            setCoremlDownloading(false)
                            toast.error('下載失敗：' + (e?.Import ?? String(e)))
                          })
                        }}
                        disabled={coremlDownloading || whisperStatus === 'running'}
                        style={{ ...inputStyle, cursor: coremlDownloading || whisperStatus === 'running' ? 'not-allowed' : 'pointer', opacity: coremlDownloading || whisperStatus === 'running' ? 0.6 : 1 }}
                      >
                        {coremlDownloading ? '下載中…' : '下載 CoreML 模型包（Apple Neural Engine）'}
                      </button>
                    )}
                    {coremlDownloading && (
                      <div style={{ marginTop: '8px' }}>
                        <div style={{ height: '4px', borderRadius: '2px', background: 'var(--color-bg-overlay)', overflow: 'hidden' }}>
                          <div style={{
                            height: '100%',
                            width: coremlTotalBytes > 0 ? `${Math.round(coremlDownloadedBytes / coremlTotalBytes * 100)}%` : '100%',
                            background: 'var(--color-accent)',
                            borderRadius: '2px',
                            transition: coremlTotalBytes > 0 ? 'width 0.3s ease' : undefined,
                            animation: coremlTotalBytes === 0 ? 'pulse 1.5s ease-in-out infinite' : undefined,
                          }} />
                        </div>
                        <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '4px' }}>
                          <span>{fmtBytes(coremlDownloadedBytes)}{coremlTotalBytes > 0 ? ` / ${fmtBytes(coremlTotalBytes)}` : ''}</span>
                          <span>
                            {coremlTotalBytes > 0 ? `${Math.round(coremlDownloadedBytes / coremlTotalBytes * 100)}%` : '連線中…'}
                            {coremlSpeedBps > 0 ? ` · ${fmtSpeed(coremlSpeedBps)}` : ''}
                          </span>
                        </div>
                      </div>
                    )}
                    <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '4px 0 0', lineHeight: 1.5 }}>
                      下載後放置於模型同目錄，重啟 whisper-server 即自動啟用 Apple Neural Engine，推論速度可提升 2–4 倍。
                      whisper-server binary 需為 CoreML 版（重新執行「自動安裝最新版」）。
                    </p>
                  </div>
                </>
              )}
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
                <p style={{ fontSize: '11px', color: 'var(--color-warning, #f59e0b)', margin: '6px 0 0', lineHeight: 1.6, padding: '6px 8px', background: 'rgba(245,158,11,0.08)', borderRadius: '5px', borderLeft: '2px solid var(--color-warning, #f59e0b)' }}>
                  <strong>Windows 用戶：</strong>若啟動失敗（exit code -1073741515），請先安裝{' '}
                  <a href="https://aka.ms/vs/17/release/vc_redist.x64.exe" target="_blank" rel="noreferrer"
                    style={{ color: 'var(--color-accent)' }}>Visual C++ Redistributable 2022 (x64)</a>。
                  <br />
                  <strong>CUDA 用戶：</strong>自動下載為 CPU 版本。若需 GPU 加速，請至{' '}
                  <a href="https://github.com/ggml-org/llama.cpp/releases" target="_blank" rel="noreferrer"
                    style={{ color: 'var(--color-accent)' }}>llama.cpp Releases</a>{' '}
                  手動下載對應 CUDA 版本（需另安裝 CUDA Toolkit）。
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

              {/* ── Embedding Server ───────────────────────────────────────── */}
              <SectionDivider />
              <ServerCard
                name="Embedding Server（語意搜尋）"
                status={embeddingStatus}
                busy={embeddingBusy}
                onStart={async () => {
                  setEmbeddingBusy(true)
                  await saveSystem(draft).catch(() => {})
                  await invoke('start_embedding_server').catch(() => {})
                  setEmbeddingBusy(false)
                }}
                onStop={async () => {
                  setEmbeddingBusy(true)
                  await invoke('stop_embedding_server').catch(() => {})
                  setEmbeddingBusy(false)
                }}
                onRestart={async () => {
                  setEmbeddingBusy(true)
                  await saveSystem(draft).catch(() => {})
                  await invoke('restart_embedding_server').catch(() => {})
                  setEmbeddingBusy(false)
                }}
              />
              <SectionDivider />
              <SectionHeader label="Embedding 模型管理" locked={embeddingStatus === 'running'} />
              <ModelDownloader
                models={EMBEDDING_MODELS}
                title="Embedding 模型"
                kind="llm"
                value={draft.embedding_model_path}
                onChange={(v) => up({ embedding_model_path: v })}
                disabled={embeddingStatus === 'running'}
              />
            </>}

            {tab === 'advanced' && <>
              {mode === 'system' && (
                <div style={{ borderTop: '1px solid var(--color-border)', paddingTop: '16px', marginTop: '8px' }}>
                  <p style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)', margin: '0 0 6px' }}>
                    資料庫修復
                  </p>
                  <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', margin: '0 0 10px', lineHeight: 1.6 }}>
                    備份設定、帳號、技能、Agent 定義及對話紀錄，重啟後自動重建資料庫並還原。重啟後搜尋索引需重新執行 reindex。
                  </p>
                  <button
                    onClick={handleRepair}
                    style={{
                      padding: '6px 16px', borderRadius: '6px', fontSize: '13px',
                      border: '1px solid var(--color-warning, #f59e0b)',
                      background: 'transparent', color: 'var(--color-warning, #f59e0b)',
                      cursor: 'pointer',
                    }}
                  >
                    修復 DB
                  </button>
                </div>
              )}
              {mode === 'personal' && (
              <div style={{ borderTop: '1px solid var(--color-border)', paddingTop: '16px', marginTop: '8px' }}>
                <ToggleRow
                  label="Agent tool 測試"
                  value={draft.show_agent_tools ?? true}
                  onChange={(v) => up({ show_agent_tools: v })}
                />
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '-8px 0 16px 0', lineHeight: 1.5 }}>
                  控制側欄是否顯示「Agent Tool 測試台」按鈕。
                </p>
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
              )}

              {/* 資料庫修復紀錄 */}
              <div style={{ borderTop: '1px solid var(--color-border)', paddingTop: '16px', marginTop: '8px' }}>
                <p style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)', margin: '0 0 10px' }}>
                  資料庫修復紀錄
                </p>
                {repairLogs.length === 0 ? (
                  <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', fontStyle: 'italic' }}>無修復紀錄</p>
                ) : (
                  <div style={{ display: 'flex', flexDirection: 'column', gap: '6px' }}>
                    {repairLogs.map((log) => (
                      <div key={log.filename} style={{ border: '1px solid var(--color-border)', borderRadius: '6px', overflow: 'hidden' }}>
                        <button
                          onClick={() => setExpandedLog(expandedLog === log.filename ? null : log.filename)}
                          style={{
                            width: '100%', textAlign: 'left', background: 'var(--color-bg-elevated)',
                            border: 'none', padding: '8px 12px', cursor: 'pointer',
                            display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: '8px',
                          }}
                        >
                          <span style={{ fontSize: '12px', color: 'var(--color-text-primary)' }}>
                            {new Date(log.timestamp).toLocaleString('zh-TW')}
                          </span>
                          <span style={{
                            fontSize: '11px', padding: '1px 6px', borderRadius: '4px',
                            background: log.result === '成功' ? 'var(--color-success-dim, #052e16)' : 'var(--color-warning-dim, #451a03)',
                            color: log.result === '成功' ? 'var(--color-success, #4ade80)' : 'var(--color-warning, #f59e0b)',
                          }}>
                            {log.result === '成功' ? '修復成功' : log.result}
                          </span>
                          <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginLeft: 'auto' }}>
                            {expandedLog === log.filename ? '▲' : '▼'}
                          </span>
                        </button>
                        {expandedLog === log.filename && (
                          <div style={{ padding: '10px 12px', fontSize: '12px', lineHeight: 1.7, color: 'var(--color-text-secondary)' }}>
                            <div><strong>原因：</strong>{log.reason}</div>
                            <div style={{ marginTop: '6px' }}><strong>已還原 tables：</strong></div>
                            {Object.keys(log.tables_restored).length === 0 ? (
                              <div style={{ color: 'var(--color-text-muted)', fontStyle: 'italic' }}>無備份可還原</div>
                            ) : (
                              <ul style={{ margin: '4px 0 0 16px', padding: 0 }}>
                                {Object.entries(log.tables_restored).map(([tbl, cnt]) => (
                                  <li key={tbl}>{tbl}：{cnt} 筆</li>
                                ))}
                              </ul>
                            )}
                          </div>
                        )}
                      </div>
                    ))}
                  </div>
                )}
              </div>
            </>}

            {tab === 'memory' && <>
              {/* Header */}
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: '4px' }}>
                <span style={{ fontSize: '13px', fontWeight: 500, color: 'var(--color-text-primary)' }}>記憶規則</span>
                {memoryRulesLoading && <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>載入中…</span>}
              </div>
              <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '0 0 14px', lineHeight: 1.5 }}>
                時間表達式與停用詞規則。AI 遇到未知時間詞時會自動新增；也可手動維護。
              </p>

              {/* Rules table */}
              {!memoryRulesLoading && memoryRules.length === 0 ? (
                <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', fontStyle: 'italic', marginBottom: '14px' }}>尚無自訂規則</p>
              ) : (
                <div style={{ border: '1px solid var(--color-border)', borderRadius: '6px', overflow: 'hidden', marginBottom: '14px' }}>
                  <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: '12px' }}>
                    <thead>
                      <tr style={{ background: 'var(--color-bg-elevated)' }}>
                        {['類型', '觸發詞', '值', '建立日期', ''].map((h) => (
                          <th key={h} style={{ textAlign: 'left', padding: '6px 10px', color: 'var(--color-text-secondary)', fontWeight: 500, borderBottom: '1px solid var(--color-border)' }}>{h}</th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {memoryRules.map((rule, i) => (
                        <tr key={rule.id} style={{ background: i % 2 === 0 ? 'transparent' : 'var(--color-bg-elevated)' }}>
                          <td style={{ padding: '6px 10px', color: 'var(--color-text-secondary)' }}>{patternTypeLabel(rule.pattern_type)}</td>
                          <td style={{ padding: '6px 10px', color: 'var(--color-text-primary)', fontFamily: 'monospace' }}>{rule.pattern}</td>
                          <td style={{ padding: '6px 10px', color: 'var(--color-text-secondary)', fontFamily: 'monospace' }}>{rule.value || '—'}</td>
                          <td style={{ padding: '6px 10px', color: 'var(--color-text-muted)', whiteSpace: 'nowrap' }}>
                            {new Date(rule.created_at).toLocaleDateString('zh-TW')}
                          </td>
                          <td style={{ padding: '6px 8px', textAlign: 'right' }}>
                            <button
                              onClick={() => handleDeleteMemoryRule(rule.id)}
                              style={{ padding: '2px 8px', borderRadius: '4px', fontSize: '11px', background: 'transparent', border: '1px solid var(--color-border)', color: 'var(--color-text-muted)', cursor: 'pointer' }}
                              onMouseEnter={e => { e.currentTarget.style.borderColor = '#e06c75'; e.currentTarget.style.color = '#e06c75' }}
                              onMouseLeave={e => { e.currentTarget.style.borderColor = 'var(--color-border)'; e.currentTarget.style.color = 'var(--color-text-muted)' }}
                            >刪除</button>
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}

              {/* Add new rule form */}
              <div style={{ border: '1px solid var(--color-border)', borderRadius: '8px', padding: '10px 12px' }}>
                <span style={{ fontSize: '11px', fontWeight: 600, color: 'var(--color-text-secondary)', display: 'block', marginBottom: '8px' }}>新增規則</span>
                <div style={{ display: 'flex', gap: '6px', alignItems: 'center', flexWrap: 'wrap' }}>
                  <select
                    value={newRuleType}
                    onChange={e => setNewRuleType(e.target.value)}
                    style={{ height: '28px', padding: '0 6px', borderRadius: '6px', border: '1px solid var(--color-border)', background: 'var(--color-bg-elevated)', color: 'var(--color-text-primary)', fontSize: '12px', outline: 'none' }}
                  >
                    <option value="temporal_exact_days">固定天數</option>
                    <option value="temporal_unit">時間單位</option>
                    <option value="stopword">停用詞</option>
                  </select>
                  <input
                    value={newRulePattern}
                    onChange={e => setNewRulePattern(e.target.value)}
                    onKeyDown={e => { if (e.key === 'Enter') handleAddMemoryRule() }}
                    placeholder="觸發詞（如「大前天」）"
                    style={{ flex: 1, minWidth: '100px', height: '28px', padding: '0 8px', borderRadius: '6px', border: '1px solid var(--color-border)', background: 'var(--color-bg-elevated)', color: 'var(--color-text-primary)', fontSize: '12px', outline: 'none' }}
                  />
                  <input
                    value={newRuleValue}
                    onChange={e => setNewRuleValue(e.target.value)}
                    onKeyDown={e => { if (e.key === 'Enter') handleAddMemoryRule() }}
                    placeholder={newRuleType === 'temporal_exact_days' ? '負整數如 -3' : newRuleType === 'temporal_unit' ? 'hours/minutes/weeks' : '（停用詞留空）'}
                    style={{ flex: 1, minWidth: '100px', height: '28px', padding: '0 8px', borderRadius: '6px', border: '1px solid var(--color-border)', background: 'var(--color-bg-elevated)', color: 'var(--color-text-primary)', fontSize: '12px', outline: 'none' }}
                  />
                  <button
                    onClick={handleAddMemoryRule}
                    style={{ padding: '4px 12px', borderRadius: '6px', fontSize: '12px', background: 'var(--color-accent-dim)', border: '1px solid var(--color-accent)', color: 'var(--color-accent)', cursor: 'pointer', whiteSpace: 'nowrap' }}
                  >+ 新增</button>
                </div>
              </div>

              {/* 立即整合記憶 */}
              <div style={{ marginTop: '20px', borderTop: '1px solid var(--color-border)', paddingTop: '16px' }}>
                <span style={{ fontSize: '13px', fontWeight: 500, color: 'var(--color-text-primary)', display: 'block', marginBottom: '6px' }}>記憶整合</span>
                <p style={{ fontSize: '11px', color: 'var(--color-text-muted)', margin: '0 0 12px', lineHeight: 1.5 }}>
                  分析未處理的對話，透過 Claude CLI 提取有價值的記憶事實。每 8 小時自動執行，也可手動觸發。
                </p>
                <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
                  <button
                    disabled={memoryAgentRunning}
                    onClick={async () => {
                      setMemoryAgentRunning(true)
                      setMemoryAgentMsg('')
                      try {
                        await invoke('trigger_memory_agent')
                        setMemoryAgentMsg('記憶整合已在背景啟動，結果將寫入 vault/log/')
                      } catch (e) {
                        setMemoryAgentMsg(`啟動失敗：${e}`)
                      } finally {
                        setMemoryAgentRunning(false)
                      }
                    }}
                    style={{
                      padding: '6px 16px', borderRadius: '6px', fontSize: '12px',
                      background: memoryAgentRunning ? 'var(--color-bg-elevated)' : 'var(--color-accent-dim)',
                      border: '1px solid var(--color-accent)',
                      color: memoryAgentRunning ? 'var(--color-text-muted)' : 'var(--color-accent)',
                      cursor: memoryAgentRunning ? 'not-allowed' : 'pointer',
                    }}
                  >
                    {memoryAgentRunning ? '啟動中…' : '立即整合記憶'}
                  </button>
                  {memoryAgentMsg && (
                    <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>{memoryAgentMsg}</span>
                  )}
                </div>
              </div>
            </>}

            {tab === 'raw' && (() => {
              const rawEntries = mode === 'system'
                ? Object.fromEntries(SYSTEM_KEYS.map(k => [k, draft[k]]))
                : Object.fromEntries(Object.entries(draft).filter(([k]) => !(SYSTEM_KEYS as readonly string[]).includes(k)))
              const label = mode === 'system' ? '機器層級設定（全域）' : '個人設定（user_settings）'
              return (
                <div>
                  <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', marginBottom: '12px' }}>
                    {label} — 未儲存的 draft 值，僅供檢閱，不可直接編輯。
                  </p>
                  <pre style={{
                    background: 'var(--color-bg-base)', border: '1px solid var(--color-border)',
                    borderRadius: '6px', padding: '14px 16px',
                    fontSize: '11.5px', color: 'var(--color-text-primary)',
                    fontFamily: "'JetBrains Mono', monospace",
                    overflowX: 'auto', whiteSpace: 'pre',
                    margin: 0, lineHeight: 1.6,
                  }}>
                    {JSON.stringify(mode === 'system' ? rawEntries : { ...rawEntries, _note: 'API Key 儲存於系統 Keychain，不顯示於此' }, null, 2)}
                  </pre>
                </div>
              )
            })()}

          </div>
        </div>

        {/* Footer */}
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '12px 20px', borderTop: '1px solid var(--color-border)', flexShrink: 0 }}>
          {mode !== 'system' && (
            <button
              onClick={handleReset}
              style={{ padding: '7px 16px', borderRadius: '6px', background: 'transparent', border: '1px solid var(--color-border)', color: 'var(--color-text-secondary)', fontSize: '13px' }}
              onMouseEnter={(e) => (e.currentTarget.style.borderColor = 'var(--color-text-muted)')}
              onMouseLeave={(e) => (e.currentTarget.style.borderColor = 'var(--color-border)')}
            >還原預設</button>
          )}
          {mode === 'system' && <div />}
          <div style={{ display: 'flex', gap: '8px' }}>
            <button
              onClick={() => onClose?.()}
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
  )

  if (inline) {
    return innerModal
  }

  return (
    <div
      style={{
        position: 'fixed', inset: 0, zIndex: 1000,
        background: 'rgba(0,0,0,0.5)',
        display: 'flex', alignItems: 'center', justifyContent: 'center',
      }}
      onClick={(e) => { if (e.target === e.currentTarget) onClose?.() }}
    >
      {/* Scope spin-button margin so it only affects this modal */}
      <style>{`
        #settings-modal input[type=number]::-webkit-inner-spin-button,
        #settings-modal input[type=number]::-webkit-outer-spin-button {
          margin-left: 4px;
          opacity: 1;
        }
      `}</style>
      {innerModal}
    </div>
  )
}
