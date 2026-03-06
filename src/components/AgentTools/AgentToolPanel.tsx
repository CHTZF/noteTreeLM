import { useState, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'

// ── Tool definitions (mirrors vault_tools() in ai.rs) ────────────────────────

type ParamType = 'text' | 'textarea' | 'number'

interface ToolParam {
  key: string
  label: string
  type: ParamType
  placeholder: string
  required: boolean
  defaultValue?: string
}

interface ToolDef {
  name: string
  label: string
  description: string
  params: ToolParam[]
  isWrite?: boolean  // write tools that modify vault
}

const TOOLS: ToolDef[] = [
  {
    name: 'search_vault',
    label: '🔍 search_vault',
    description: '全文搜索 Vault 中的筆記，返回相關筆記列表及摘要',
    params: [
      { key: 'query', label: '關鍵字 (query)', type: 'text', placeholder: '例如：設定系統', required: true },
    ],
  },
  {
    name: 'list_structure',
    label: '📁 list_structure',
    description: '列出指定資料夾路徑下的子資料夾和筆記（.md）',
    params: [
      { key: 'path', label: '路徑 (path)', type: 'text', placeholder: '空字串 = 根目錄', required: true, defaultValue: '' },
    ],
  },
  {
    name: 'read_note',
    label: '📄 read_note',
    description: '讀取指定筆記的完整 Markdown 內容',
    params: [
      { key: 'path', label: '筆記路徑 (path)', type: 'text', placeholder: 'folder/note.md', required: true },
    ],
  },
  {
    name: 'create_note',
    label: '✏️ create_note',
    description: '在 Vault 中建立新筆記（會自動建立所需的父資料夾）',
    params: [
      { key: 'path', label: '筆記路徑 (path)', type: 'text', placeholder: 'folder/test_note.md', required: true },
      { key: 'content', label: '內容 (content)', type: 'textarea', placeholder: '# 標題\n\n內容', required: true },
    ],
    isWrite: true,
  },
  {
    name: 'update_note',
    label: '📝 update_note',
    description: '覆寫更新現有筆記的完整內容',
    params: [
      { key: 'path', label: '筆記路徑 (path)', type: 'text', placeholder: 'folder/note.md', required: true },
      { key: 'content', label: '新內容 (content)', type: 'textarea', placeholder: '# 標題\n\n更新後的內容', required: true },
    ],
    isWrite: true,
  },
  {
    name: 'create_folder',
    label: '📂 create_folder',
    description: '在 Vault 中建立新資料夾（含所有中間層資料夾）',
    params: [
      { key: 'path', label: '資料夾路徑 (path)', type: 'text', placeholder: 'test_folder/subfolder', required: true },
    ],
    isWrite: true,
  },
  {
    name: 'query_memory',
    label: '🧠 query_memory',
    description: '查詢過去整理的對話記憶筆記',
    params: [
      { key: 'keywords', label: '關鍵字，逗號分隔 (keywords)', type: 'text', placeholder: 'Rust, async, Tauri', required: true },
      { key: 'since', label: '起始日期，可選 (since)', type: 'text', placeholder: 'YYYY-MM-DD', required: false },
      { key: 'limit', label: '最多筆數，可選 (limit)', type: 'number', placeholder: '3', required: false },
    ],
  },
  {
    name: 'call_external_ai',
    label: '🌐 call_external_ai',
    description: '呼叫外部 AI 服務（如 OpenAI / Anthropic）獲取即時資訊',
    params: [
      { key: 'query', label: '問題 (query)', type: 'text', placeholder: '今天的天氣如何？', required: true },
    ],
  },
  {
    name: 'open_note',
    label: '🗂 open_note',
    description: '在筆記編輯器中打開（切換至）指定筆記',
    params: [
      { key: 'path', label: '筆記路徑 (path)', type: 'text', placeholder: 'folder/note.md', required: true },
    ],
  },
]

// ── ToolCard ──────────────────────────────────────────────────────────────────

interface ToolCardProps {
  tool: ToolDef
}

function ToolCard({ tool }: ToolCardProps) {
  const [expanded, setExpanded] = useState(false)
  const [values, setValues] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {}
    tool.params.forEach(p => { init[p.key] = p.defaultValue ?? '' })
    return init
  })
  const [loading, setLoading] = useState(false)
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null)

  const buildArgs = useCallback((): Record<string, unknown> => {
    const args: Record<string, unknown> = {}
    tool.params.forEach(p => {
      const v = values[p.key].trim()
      if (!v && !p.required) return
      if (p.key === 'keywords') {
        // Convert comma-separated string to array for query_memory
        args[p.key] = v.split(',').map(s => s.trim()).filter(Boolean)
      } else if (p.type === 'number' && v) {
        args[p.key] = Number(v)
      } else {
        args[p.key] = v
      }
    })
    return args
  }, [tool.params, values])

  const handleTest = useCallback(async () => {
    setLoading(true)
    setResult(null)
    try {
      const args = buildArgs()
      // 30s timeout so the button never freezes permanently
      const res = await Promise.race([
        invoke<string>('test_vault_tool', { toolName: tool.name, args }),
        new Promise<never>((_, reject) =>
          setTimeout(() => reject(new Error('逾時（30s），請確認 Vault 已設定且伺服器正常')), 30_000)
        ),
      ])
      setResult({ ok: true, text: res })
    } catch (e) {
      setResult({ ok: false, text: String(e) })
    } finally {
      setLoading(false)
    }
  }, [tool.name, buildArgs])

  const canTest = tool.params
    .filter(p => p.required)
    .every(p => values[p.key].trim().length > 0)

  return (
    <div style={{
      borderRadius: '8px',
      border: '1px solid var(--color-border)',
      overflow: 'hidden',
      background: 'var(--color-bg-secondary, var(--color-bg-hover))',
    }}>
      {/* Header row */}
      <button
        onClick={() => setExpanded(e => !e)}
        style={{
          width: '100%', padding: '10px 12px',
          display: 'flex', alignItems: 'center', justifyContent: 'space-between',
          background: 'transparent', cursor: 'pointer', textAlign: 'left',
          gap: '8px',
        }}
        onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
        onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: '8px', minWidth: 0 }}>
          <span style={{ fontSize: '13px', fontWeight: 600, color: 'var(--color-text-primary)', fontFamily: 'monospace', flexShrink: 0 }}>
            {tool.label}
          </span>
          {tool.isWrite && (
            <span style={{
              fontSize: '10px', padding: '1px 5px', borderRadius: '3px',
              background: 'var(--color-warning, #f59e0b)', color: 'white',
              flexShrink: 0,
            }}>
              寫入
            </span>
          )}
          {!expanded && (
            <span style={{ fontSize: '12px', color: 'var(--color-text-muted)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
              {tool.description}
            </span>
          )}
        </div>
        <span style={{ color: 'var(--color-text-muted)', fontSize: '11px', flexShrink: 0 }}>
          {expanded ? '▲' : '▼'}
        </span>
      </button>

      {/* Expanded body */}
      {expanded && (
        <div style={{ padding: '0 12px 12px', display: 'flex', flexDirection: 'column', gap: '10px' }}>
          <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', margin: 0, lineHeight: 1.5 }}>
            {tool.description}
          </p>

          {/* Parameter inputs */}
          {tool.params.map(param => (
            <div key={param.key} style={{ display: 'flex', flexDirection: 'column', gap: '4px' }}>
              <label style={{ fontSize: '11px', color: 'var(--color-text-secondary)', fontFamily: 'monospace' }}>
                {param.label}
                {param.required && <span style={{ color: 'var(--color-warning, #f59e0b)', marginLeft: '2px' }}>*</span>}
              </label>
              {param.type === 'textarea' ? (
                <textarea
                  value={values[param.key]}
                  onChange={e => setValues(prev => ({ ...prev, [param.key]: e.target.value }))}
                  placeholder={param.placeholder}
                  rows={4}
                  style={{
                    resize: 'vertical', fontFamily: 'monospace', fontSize: '12px',
                    padding: '6px 8px', borderRadius: '5px',
                    border: '1px solid var(--color-border)',
                    background: 'var(--color-bg-base)',
                    color: 'var(--color-text-primary)',
                    width: '100%', boxSizing: 'border-box',
                  }}
                />
              ) : (
                <input
                  type={param.type === 'number' ? 'number' : 'text'}
                  value={values[param.key]}
                  onChange={e => setValues(prev => ({ ...prev, [param.key]: e.target.value }))}
                  placeholder={param.placeholder}
                  style={{
                    fontFamily: param.type === 'number' ? undefined : 'monospace',
                    fontSize: '12px', padding: '5px 8px', borderRadius: '5px',
                    border: '1px solid var(--color-border)',
                    background: 'var(--color-bg-base)',
                    color: 'var(--color-text-primary)',
                    width: '100%', boxSizing: 'border-box',
                  }}
                />
              )}
            </div>
          ))}

          {/* Test button */}
          <button
            onClick={handleTest}
            disabled={!canTest || loading}
            style={{
              padding: '6px 14px', borderRadius: '6px', fontSize: '12px',
              alignSelf: 'flex-start',
              background: canTest && !loading ? 'var(--color-accent)' : 'var(--color-border)',
              color: canTest && !loading ? 'white' : 'var(--color-text-muted)',
              cursor: canTest && !loading ? 'pointer' : 'not-allowed',
              fontWeight: 600,
            }}
          >
            {loading ? '執行中…' : '▶ 測試'}
          </button>

          {/* Result */}
          {result && (
            <div style={{
              padding: '8px 10px', borderRadius: '6px', fontSize: '12px',
              fontFamily: 'monospace', lineHeight: 1.6,
              whiteSpace: 'pre-wrap', wordBreak: 'break-word',
              maxHeight: '240px', overflowY: 'auto',
              background: result.ok
                ? 'var(--color-bg-base)'
                : 'rgba(239,68,68,0.08)',
              border: `1px solid ${result.ok ? 'var(--color-border)' : 'rgba(239,68,68,0.3)'}`,
              color: result.ok ? 'var(--color-text-primary)' : '#ef4444',
            }}>
              <span style={{ fontSize: '10px', color: result.ok ? 'var(--color-success, #22c55e)' : '#ef4444', fontWeight: 700, display: 'block', marginBottom: '4px' }}>
                {result.ok ? '✅ 成功' : '❌ 錯誤'}
              </span>
              {result.text}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

// ── AgentToolPanel (modal overlay) ───────────────────────────────────────────

interface AgentToolPanelProps {
  onClose: () => void
}

export default function AgentToolPanel({ onClose }: AgentToolPanelProps) {
  return (
    <div
      style={{
        position: 'fixed', inset: 0,
        background: 'rgba(0,0,0,0.5)', backdropFilter: 'blur(2px)',
        zIndex: 1000, display: 'flex', alignItems: 'center', justifyContent: 'center',
      }}
      onClick={e => { if (e.target === e.currentTarget) onClose() }}
    >
      <div style={{
        width: '560px', maxWidth: '95vw',
        maxHeight: '85vh',
        background: 'var(--color-bg-base)',
        borderRadius: '12px',
        border: '1px solid var(--color-border)',
        display: 'flex', flexDirection: 'column',
        overflow: 'hidden',
        boxShadow: '0 20px 60px rgba(0,0,0,0.3)',
      }}>
        {/* Header */}
        <div style={{
          display: 'flex', alignItems: 'center', justifyContent: 'space-between',
          padding: '14px 16px', borderBottom: '1px solid var(--color-border)',
          flexShrink: 0,
        }}>
          <div>
            <span style={{ fontSize: '14px', fontWeight: 700, color: 'var(--color-text-primary)' }}>
              ⚡ Agent Tool 測試台
            </span>
            <span style={{ fontSize: '12px', color: 'var(--color-text-muted)', marginLeft: '8px' }}>
              {TOOLS.length} 個工具
            </span>
          </div>
          <button
            onClick={onClose}
            style={{
              fontSize: '18px', color: 'var(--color-text-muted)', padding: '2px 6px',
              borderRadius: '5px', cursor: 'pointer',
            }}
            onMouseEnter={e => (e.currentTarget.style.color = 'var(--color-text-primary)')}
            onMouseLeave={e => (e.currentTarget.style.color = 'var(--color-text-muted)')}
          >
            ✕
          </button>
        </div>

        {/* Scroll area */}
        <div style={{ flex: 1, overflowY: 'auto', padding: '12px', display: 'flex', flexDirection: 'column', gap: '8px' }}>
          {TOOLS.map(tool => (
            <ToolCard key={tool.name} tool={tool} />
          ))}
        </div>
      </div>
    </div>
  )
}
