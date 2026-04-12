import { useState, useEffect, useRef, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { open as openDialog } from '@tauri-apps/plugin-dialog'

// ── Types ──────────────────────────────────────────────────────────────────────

interface DownloadProgress {
  model_id: string
  downloaded_bytes: number
  total_bytes: number
  speed_bps: number
  status: 'downloading' | 'completed' | 'error' | 'cancelled'
  file_path?: string
  error?: string
}

interface ModelFileInfo {
  model_id: string
  filename: string
  file_path: string
  size_bytes: number
  is_partial: boolean
}

export interface ModelItem {
  id: string
  filename: string
  name: string
  displaySize: string
  desc: string
  url: string
  recommended?: boolean
  /** True when the model has built-in chain-of-thought (e.g. Qwen3.5).
   *  When selected, the think tool injection (enable_think) is skipped. */
  nativeThink?: boolean
  /** Model's native context window in tokens. Used to set --ctx-size on llama-server
   *  and to scale the system's ContextBudget proportionally. */
  contextSize?: number
}

// Represents a model ready for use (complete + in models dir)
interface AvailableModel {
  key: string          // unique identifier (filename or full external path)
  path: string         // absolute path to use
  displayName: string  // short label shown in dropdown / 已下載 list
  sizeBytesStr?: string
  isLegacyExternal?: boolean  // backward compat: path not in models dir
}

// ── Model catalogues ───────────────────────────────────────────────────────────

export const WHISPER_MODELS: ModelItem[] = [
  {
    id: 'ggml-tiny',
    filename: 'ggml-tiny.bin',
    name: 'Tiny',
    displaySize: '75 MB',
    desc: '最快速，精度較低，適合測試',
    url: 'https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin',
  },
  {
    id: 'ggml-base',
    filename: 'ggml-base.bin',
    name: 'Base',
    displaySize: '142 MB',
    desc: '快速，適合輕量使用',
    url: 'https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin',
  },
  {
    id: 'ggml-small',
    filename: 'ggml-small.bin',
    name: 'Small',
    displaySize: '466 MB',
    desc: '平衡速度與精度',
    url: 'https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin',
  },
  {
    id: 'ggml-large-v3-turbo-q5_0',
    filename: 'ggml-large-v3-turbo-q5_0.bin',
    name: 'Large-v3-turbo (Q5)',
    displaySize: '547 MB',
    desc: '推薦：最佳中文支援，高速推理',
    url: 'https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin',
    recommended: true,
  },
  {
    id: 'ggml-large-v3',
    filename: 'ggml-large-v3.bin',
    name: 'Large-v3',
    displaySize: '3.1 GB',
    desc: '最高精度，需要較多記憶體',
    url: 'https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin',
  },
  {
    id: 'breeze-asr-25-q4',
    filename: 'ggml-model-q4_k.bin',
    name: 'MR Breeze ASR 25 (Q4)',
    displaySize: '889 MB',
    desc: '繁體中文優化，MediaTek Research 出品，中英混合辨識表現優異',
    url: 'https://huggingface.co/alan314159/Breeze-ASR-25-whispercpp/resolve/main/ggml-model-q4_k.bin',
    recommended: true,
  },
]

export const LLM_MODELS: ModelItem[] = [
  {
    id: 'Qwen2.5-1.5B-Instruct-Q4_K_M',
    filename: 'Qwen2.5-1.5B-Instruct-Q4_K_M.gguf',
    name: 'Qwen2.5-1.5B (Q4)',
    displaySize: '~1 GB',
    desc: '最快速，適合低記憶體環境（4 GB RAM）',
    url: 'https://huggingface.co/bartowski/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/Qwen2.5-1.5B-Instruct-Q4_K_M.gguf',
    contextSize: 32768,
  },
  {
    id: 'Qwen2.5-3B-Instruct-Q4_K_M',
    filename: 'Qwen2.5-3B-Instruct-Q4_K_M.gguf',
    name: 'Qwen2.5-3B (Q4)',
    displaySize: '~2 GB',
    desc: '平衡速度與品質，適合 8 GB RAM',
    url: 'https://huggingface.co/bartowski/Qwen2.5-3B-Instruct-GGUF/resolve/main/Qwen2.5-3B-Instruct-Q4_K_M.gguf',
    contextSize: 32768,
  },
  {
    id: 'Qwen2.5-7B-Instruct-Q4_K_M',
    filename: 'Qwen2.5-7B-Instruct-Q4_K_M.gguf',
    name: 'Qwen2.5-7B (Q4)',
    displaySize: '~4.7 GB',
    desc: '推薦：高品質輸出，適合 16 GB RAM',
    url: 'https://huggingface.co/bartowski/Qwen2.5-7B-Instruct-GGUF/resolve/main/Qwen2.5-7B-Instruct-Q4_K_M.gguf',
    recommended: true,
    contextSize: 32768,
  },
  {
    id: 'Qwen2.5-14B-Instruct-Q4_K_M',
    filename: 'Qwen2.5-14B-Instruct-Q4_K_M.gguf',
    name: 'Qwen2.5-14B (Q4)',
    displaySize: '~9 GB',
    desc: '最高品質，需要 16 GB+ RAM 或獨立顯卡',
    url: 'https://huggingface.co/bartowski/Qwen2.5-14B-Instruct-GGUF/resolve/main/Qwen2.5-14B-Instruct-Q4_K_M.gguf',
    contextSize: 32768,
  },
  {
    id: 'Qwen3.5-0.8B-Q4_K_M',
    filename: 'Qwen3.5-0.8B-Q4_K_M.gguf',
    name: 'Qwen3.5-0.8B (Q4) — Draft',
    displaySize: '~0.5 GB',
    desc: 'Speculative Decoding 用 draft model，與 Qwen3.5-9B 搭配可加速 2-2.5x，建議一起下載',
    url: 'https://huggingface.co/unsloth/Qwen3.5-0.8B-GGUF/resolve/main/Qwen3.5-0.8B-Q4_K_M.gguf',
    nativeThink: true,
    contextSize: 32768,
  },
  {
    id: 'Qwen3.5-9B-Q4_K_M',
    filename: 'Qwen3.5-9B-Q4_K_M.gguf',
    name: 'Qwen3.5-9B (Q4)',
    displaySize: '~5.2 GB',
    desc: '新世代旗艦：原生思考模式、工具呼叫、201 語言，Q4 量化速度快 30-40%，建議 16 GB RAM',
    url: 'https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/main/Qwen3.5-9B-Q4_K_M.gguf',
    recommended: true,
    nativeThink: true,
    contextSize: 32768,
  },
  {
    id: 'Qwen3.5-9B-Q6_K',
    filename: 'Qwen3.5-9B-Q6_K.gguf',
    name: 'Qwen3.5-9B (Q6)',
    displaySize: '~7.5 GB',
    desc: '新世代旗艦：原生思考模式、工具呼叫、201 語言，Q6 近乎無損，建議 24 GB RAM',
    url: 'https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/main/Qwen3.5-9B-Q6_K.gguf',
    nativeThink: true,
    contextSize: 32768,
  },
  {
    id: 'gemma-4-E4B-Q4_K_M',
    filename: 'google_gemma-4-E4B-it-Q4_K_M.gguf',
    name: 'Gemma 4 E4B (Q4)',
    displaySize: '~5.4 GB',
    desc: 'Google 新世代：4.5B 有效參數、256K context、原生思考模式，工具呼叫品質優於 Qwen 系列，建議 16 GB+ RAM',
    url: 'https://huggingface.co/bartowski/google_gemma-4-E4B-it-GGUF/resolve/main/google_gemma-4-E4B-it-Q4_K_M.gguf',
    nativeThink: true,
    contextSize: 131072,
  },
]

export const EMBEDDING_MODELS: ModelItem[] = [
  {
    id: 'bge-m3-q4_k_m',
    filename: 'bge-m3-q4_k_m.gguf',
    name: 'BGE-M3 (Q4)',
    displaySize: '~1.2 GB',
    desc: '推薦：BAAI 多語言 embedding 模型，支援中文、英文，適合語意搜尋',
    url: 'https://huggingface.co/gpustack/bge-m3-GGUF/resolve/main/bge-m3-Q4_K_M.gguf',
    recommended: true,
  },
]

// ── Formatters ─────────────────────────────────────────────────────────────────

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

// ── Component ──────────────────────────────────────────────────────────────────

interface Props {
  /** Downloadable model catalogue */
  models: ModelItem[]
  /** Section title (shown above 已下載模型) */
  title: string
  /** 'whisper' filters .bin files; 'llm' filters .gguf files */
  kind: 'whisper' | 'llm'
  /** Currently selected model path (bound to settings field) */
  value: string
  /** Called when the user selects a model or imports one */
  onChange: (path: string) => void
  /** When true, locks model selection and import (server is running) */
  disabled?: boolean
}

export default function ModelDownloader({ models, title, kind, value, onChange, disabled = false }: Props) {
  const [dirInfos, setDirInfos] = useState<ModelFileInfo[]>([])
  const [progress, setProgress] = useState<Record<string, DownloadProgress>>({})
  const unlistenRef = useRef<UnlistenFn | null>(null)
  // ref tracks active import filename so the progress listener can react without stale closure
  const importingFilenameRef = useRef<string | null>(null)

  const ext = kind === 'whisper' ? '.bin' : '.gguf'

  // Complete (non-partial) models in models dir that match our extension
  const completeDirModels = dirInfos.filter(m => !m.is_partial && m.filename.endsWith(ext))

  // All available models; if current value is not in managed dir, add as legacy entry for backward compat
  const availableModels: AvailableModel[] = [
    ...completeDirModels.map(m => ({
      key: m.filename,
      path: m.file_path,
      displayName: m.filename,
      sizeBytesStr: fmtBytes(m.size_bytes),
    })),
    // Backward compat: current value not in models dir (e.g. previously set via path picker)
    ...(value && !completeDirModels.some(m => m.file_path === value)
      ? [{ key: value, path: value, displayName: (value.split('/').pop() || value) + ' (外部路徑)', isLegacyExternal: true }]
      : []),
  ]

  const refreshDir = useCallback(async () => {
    try {
      const infos = await invoke<ModelFileInfo[]>('get_downloaded_models')
      setDirInfos(infos)
    } catch (e) {
      console.error('get_downloaded_models failed:', e)
    }
  }, [])

  useEffect(() => {
    refreshDir()

    let cancelled = false
    listen<DownloadProgress>('model-download-progress', (event) => {
      if (cancelled) return
      const prog = event.payload
      setProgress(prev => ({ ...prev, [prog.model_id]: prog }))
      if (prog.status === 'completed') {
        refreshDir()
        // If this was an import we initiated, auto-select the copied model
        if (importingFilenameRef.current === prog.model_id && prog.file_path) {
          importingFilenameRef.current = null
          onChange(prog.file_path)
        }
      }
      if (['completed', 'cancelled', 'error'].includes(prog.status)) {
        if (importingFilenameRef.current === prog.model_id) {
          importingFilenameRef.current = null
        }
        setTimeout(() => {
          if (cancelled) return
          setProgress(prev => { const n = { ...prev }; delete n[prog.model_id]; return n })
        }, 3000)
      }
    }).then(fn => {
      if (cancelled) fn()
      else unlistenRef.current = fn
    })

    return () => {
      cancelled = true
      unlistenRef.current?.()
    }
  }, [refreshDir, onChange])

  // ── Handlers ─────────────────────────────────────────────────────────────────

  const handleImport = async () => {
    try {
      const file = await openDialog({
        multiple: false,
        filters: [{ name: title, extensions: [ext.slice(1)] }],
      })
      if (!file) return
      const srcPath = typeof file === 'string' ? file : (file as any).path ?? String(file)
      const filename = srcPath.split('/').pop() || srcPath.split('\\').pop() || srcPath

      // Track import so the progress listener can auto-select on completion
      importingFilenameRef.current = filename

      // Kick off background copy; returns immediately
      await invoke('import_model_file', { srcPath })
    } catch (e) {
      importingFilenameRef.current = null
      console.error('import_model_file failed:', e)
    }
  }

  const handleDeleteDir = async (info: ModelFileInfo) => {
    try {
      await invoke('delete_model_file', { filename: info.filename })
      if (value === info.file_path) onChange('')
      await refreshDir()
    } catch (e) {
      console.error('delete_model_file failed:', e)
    }
  }

  const handleDownload = async (model: ModelItem) => {
    try {
      await invoke('start_model_download', {
        modelId: model.id,
        url: model.url,
        filename: model.filename,
      })
    } catch (e) {
      console.error('start_model_download failed:', e)
    }
  }

  const handleCancel = async (modelId: string) => {
    try {
      await invoke('cancel_model_download', { modelId })
    } catch (e) {
      console.error('cancel_model_download failed:', e)
    }
  }

  // ── Styles ────────────────────────────────────────────────────────────────────

  const sectionLabel: React.CSSProperties = {
    fontSize: '12px', color: 'var(--color-text-secondary)',
    marginBottom: '8px', display: 'flex', alignItems: 'center', justifyContent: 'space-between',
  }
  const smallBtn = (accent = false): React.CSSProperties => ({
    padding: '3px 9px', borderRadius: '4px', fontSize: '11px',
    background: accent ? 'var(--color-accent)' : 'var(--color-bg-overlay)',
    color: accent ? '#fff' : 'var(--color-text-secondary)',
    border: accent ? 'none' : '1px solid var(--color-border)',
    cursor: 'pointer',
  })

  // ── Render ────────────────────────────────────────────────────────────────────

  // Models currently being imported (not in catalogue, progress tracked by filename)
  const importingEntries = Object.entries(progress).filter(([modelId, p]) =>
    p.status === 'downloading' &&
    !models.some(m => m.id === modelId) &&
    modelId.endsWith(ext)
  )

  return (
    <div style={{ marginBottom: '20px' }}>

      {/* ─── 已下載模型 ─────────────────────────────────────────────────────── */}
      <div style={sectionLabel}>
        <span style={{ fontWeight: 500 }}>{title} — 已下載模型</span>
        <button
          onClick={handleImport}
          disabled={disabled}
          style={{ ...smallBtn(), ...(disabled ? { opacity: 0.4, cursor: 'not-allowed' } : {}) }}
        >
          + 匯入模型
        </button>
      </div>

      <div style={{
        border: '1px solid var(--color-border)', borderRadius: '8px',
        overflow: 'hidden', marginBottom: '14px',
      }}>
        {/* Import-in-progress rows (copying large files) */}
        {importingEntries.map(([filename, prog]) => {
          const pct = prog.total_bytes > 0 ? Math.round(prog.downloaded_bytes / prog.total_bytes * 100) : -1
          return (
            <div key={filename} style={{
              padding: '8px 14px',
              background: 'var(--color-bg-elevated)',
              borderBottom: '1px solid var(--color-border)',
            }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                <span style={{ fontSize: '12px', color: 'var(--color-text-secondary)', flex: 1 }}>
                  {filename}
                </span>
                <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>
                  複製中{pct >= 0 ? ` ${pct}%` : '…'}
                  {prog.speed_bps > 0 ? ` · ${fmtSpeed(prog.speed_bps)}` : ''}
                </span>
              </div>
              {pct >= 0 && (
                <div style={{ marginTop: '6px', height: '3px', borderRadius: '2px', background: 'var(--color-bg-overlay)', overflow: 'hidden' }}>
                  <div style={{ height: '100%', width: `${pct}%`, background: 'var(--color-accent)', borderRadius: '2px', transition: 'width 0.3s ease' }} />
                </div>
              )}
            </div>
          )
        })}

        {availableModels.length === 0 && importingEntries.length === 0 ? (
          <div style={{
            padding: '12px 14px', fontSize: '12px',
            color: 'var(--color-text-muted)',
            background: 'var(--color-bg-elevated)',
          }}>
            尚無已下載模型
          </div>
        ) : availableModels.map((am, i) => (
          <div key={am.key} style={{
            padding: '8px 14px',
            borderTop: i > 0 || importingEntries.length > 0 ? '1px solid var(--color-border)' : 'none',
            background: value === am.path ? 'rgba(124,140,248,0.06)' : 'var(--color-bg-elevated)',
            display: 'flex', alignItems: 'center', gap: '8px',
          }}>
            <div style={{ flex: 1, minWidth: 0 }}>
              <span style={{
                fontSize: '12px', fontWeight: 500,
                color: value === am.path ? 'var(--color-accent)' : 'var(--color-text-primary)',
              }}>{am.displayName}</span>
              {am.sizeBytesStr && (
                <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginLeft: '8px' }}>
                  {am.sizeBytesStr}
                </span>
              )}
            </div>
            {!am.isLegacyExternal && (
              <button
                onClick={() => handleDeleteDir(dirInfos.find(d => d.file_path === am.path)!)}
                style={smallBtn()}
                title="刪除此模型"
              >刪除</button>
            )}
          </div>
        ))}
      </div>

      {/* ─── 選擇使用的模型（下拉選單）──────────────────────────────────────── */}
      <div style={{ marginBottom: '18px' }}>
        <div style={{ fontSize: '12px', color: 'var(--color-text-secondary)', marginBottom: '6px' }}>
          選擇使用的模型
        </div>
        {availableModels.length === 0 ? (
          <div style={{
            height: '32px', padding: '0 10px', borderRadius: '6px', fontSize: '13px',
            border: '1px solid var(--color-border)', boxSizing: 'border-box',
            background: 'var(--color-bg-elevated)', color: 'var(--color-text-muted)',
            display: 'flex', alignItems: 'center',
          }}>
            尚無可用模型，請先下載或匯入模型
          </div>
        ) : (
          <select
            value={value}
            disabled={disabled}
            onChange={e => onChange(e.target.value)}
            style={{
              width: '100%', height: '32px', padding: '0 10px', boxSizing: 'border-box',
              background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
              borderRadius: '6px', color: 'var(--color-text-primary)', fontSize: '13px', outline: 'none',
              opacity: disabled ? 0.4 : 1, cursor: disabled ? 'not-allowed' : 'default',
            }}
          >
            <option value="">請選擇模型…</option>
            {availableModels.map(am => (
              <option key={am.key} value={am.path}>
                {am.displayName}{am.sizeBytesStr ? ` (${am.sizeBytesStr})` : ''}
              </option>
            ))}
          </select>
        )}
      </div>

      {/* ─── 下載更多模型 ──────────────────────────────────────────────────── */}
      <div style={{ fontSize: '12px', color: 'var(--color-text-secondary)', marginBottom: '8px' }}>
        <span>下載更多模型</span>
        <span style={{ color: 'var(--color-text-muted)', fontStyle: 'italic', marginLeft: '8px' }}>
          存入應用程式資料目錄
        </span>
      </div>

      <div style={{ border: '1px solid var(--color-border)', borderRadius: '8px', overflow: 'hidden' }}>
        {models.map((model, index) => {
          const dirInfo = dirInfos.find(d => d.filename === model.filename)
          const prog = progress[model.id]
          const isDownloading = prog?.status === 'downloading'
          const isComplete = !!dirInfo && !dirInfo.is_partial
          const isPartial = dirInfo?.is_partial === true
          const pct = prog && prog.total_bytes > 0
            ? Math.round((prog.downloaded_bytes / prog.total_bytes) * 100)
            : 0

          return (
            <div key={model.id} style={{
              padding: '10px 14px',
              borderTop: index > 0 ? '1px solid var(--color-border)' : 'none',
              background: isComplete ? 'rgba(124,140,248,0.03)' : 'var(--color-bg-elevated)',
            }}>
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: '8px' }}>
                <div style={{ minWidth: 0 }}>
                  <span style={{ fontSize: '13px', fontWeight: 500, color: 'var(--color-text-primary)' }}>
                    {model.name}
                  </span>
                  {model.recommended && (
                    <span style={{
                      fontSize: '10px', color: 'var(--color-accent)',
                      background: 'var(--color-accent-dim)',
                      padding: '1px 5px', borderRadius: '3px',
                      marginLeft: '6px', fontWeight: 500,
                    }}>推薦</span>
                  )}
                  <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginLeft: '8px' }}>
                    {model.displaySize}
                  </span>
                </div>

                <div style={{ display: 'flex', gap: '6px', flexShrink: 0 }}>
                  {isComplete && (
                    <span style={{ fontSize: '11px', color: 'var(--color-success)', fontWeight: 500 }}>
                      已下載 ✓
                    </span>
                  )}
                  {!isComplete && !isDownloading && (
                    <button onClick={() => handleDownload(model)} style={smallBtn()}>
                      {isPartial ? '繼續下載' : '下載'}
                    </button>
                  )}
                  {isDownloading && (
                    <button onClick={() => handleCancel(model.id)} style={smallBtn()}>取消</button>
                  )}
                </div>
              </div>

              <p style={{ margin: '3px 0 0 0', fontSize: '11px', color: 'var(--color-text-muted)', lineHeight: 1.4 }}>
                {model.desc}
              </p>

              {isPartial && !isDownloading && (
                <p style={{ margin: '4px 0 0 0', fontSize: '11px', color: 'var(--color-warning)' }}>
                  已下載 {fmtBytes(dirInfo.size_bytes)}（未完成，可繼續下載）
                </p>
              )}

              {isDownloading && (
                <div style={{ marginTop: '8px' }}>
                  <div style={{ height: '4px', borderRadius: '2px', background: 'var(--color-bg-overlay)', overflow: 'hidden' }}>
                    <div style={{ height: '100%', width: `${pct}%`, background: 'var(--color-accent)', borderRadius: '2px', transition: 'width 0.3s ease' }} />
                  </div>
                  <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: '11px', color: 'var(--color-text-muted)', marginTop: '4px' }}>
                    <span>
                      {fmtBytes(prog!.downloaded_bytes)}
                      {prog!.total_bytes > 0 ? ` / ${fmtBytes(prog!.total_bytes)}` : ''}
                    </span>
                    <span>
                      {prog!.total_bytes > 0 ? `${pct}%` : '下載中…'}
                      {prog!.speed_bps > 0 ? ` · ${fmtSpeed(prog!.speed_bps)}` : ''}
                    </span>
                  </div>
                </div>
              )}

              {prog?.status === 'error' && (
                <p style={{ margin: '4px 0 0 0', fontSize: '11px', color: 'var(--color-error, #ef4444)' }}>
                  下載失敗：{prog.error}
                </p>
              )}
              {prog?.status === 'cancelled' && (
                <p style={{ margin: '4px 0 0 0', fontSize: '11px', color: 'var(--color-text-muted)' }}>
                  已取消（進度保留，可繼續下載）
                </p>
              )}
            </div>
          )
        })}
      </div>
    </div>
  )
}
