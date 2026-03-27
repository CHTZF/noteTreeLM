import { useState, useCallback, useEffect, useRef } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

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
      { key: 'path', label: '路徑 (path)', type: 'text', placeholder: '空字串 = 根目錄', required: false, defaultValue: '' },
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
    name: 'open_note',
    label: '🗂 open_note',
    description: '在筆記編輯器中打開（切換至）指定筆記',
    params: [
      { key: 'path', label: '筆記路徑 (path)', type: 'text', placeholder: 'folder/note.md', required: true },
    ],
  },
]

// ── Shared helpers ─────────────────────────────────────────────────────────────

function getDefaultParams(tool: ToolDef): Record<string, string> {
  const init: Record<string, string> = {}
  tool.params.forEach(p => { init[p.key] = p.defaultValue ?? '' })
  return init
}

function buildInvokeArgs(tool: ToolDef, params: Record<string, string>): Record<string, unknown> {
  const args: Record<string, unknown> = {}
  tool.params.forEach(p => {
    const v = (params[p.key] ?? '').trim()
    // 非必填且無明確 defaultValue（undefined）才跳過空值；有 defaultValue（含 ''）的始終包含
    if (!v && !p.required && p.defaultValue === undefined) return
    if (p.key === 'keywords') {
      args[p.key] = v.split(',').map(s => s.trim()).filter(Boolean)
    } else if (p.type === 'number' && v) {
      args[p.key] = Number(v)
    } else {
      args[p.key] = v
    }
  })
  return args
}

const inputStyle: React.CSSProperties = {
  fontFamily: 'monospace',
  fontSize: '12px',
  padding: '5px 8px',
  borderRadius: '5px',
  border: '1px solid var(--color-border)',
  background: 'var(--color-bg-base)',
  color: 'var(--color-text-primary)',
  width: '100%',
  boxSizing: 'border-box',
}

// ── ToolCard (個別測試) ──────────────────────────────────────────────────────

interface ToolCardProps {
  tool: ToolDef
}

function ToolCard({ tool }: ToolCardProps) {
  const [expanded, setExpanded] = useState(false)
  const [values, setValues] = useState<Record<string, string>>(() => getDefaultParams(tool))
  const [loading, setLoading] = useState(false)
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null)

  const handleTest = useCallback(async () => {
    setLoading(true)
    setResult(null)
    try {
      const args = buildInvokeArgs(tool, values)
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
  }, [tool, values])

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

      {expanded && (
        <div style={{ padding: '0 12px 12px', display: 'flex', flexDirection: 'column', gap: '10px' }}>
          <p style={{ fontSize: '12px', color: 'var(--color-text-muted)', margin: 0, lineHeight: 1.5 }}>
            {tool.description}
          </p>

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
                  style={{ ...inputStyle, resize: 'vertical' }}
                />
              ) : (
                <input
                  type={param.type === 'number' ? 'number' : 'text'}
                  value={values[param.key]}
                  onChange={e => setValues(prev => ({ ...prev, [param.key]: e.target.value }))}
                  placeholder={param.placeholder}
                  style={{ ...inputStyle, fontFamily: param.type === 'number' ? undefined : 'monospace' }}
                />
              )}
            </div>
          ))}

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

          {result && (
            <div style={{
              padding: '8px 10px', borderRadius: '6px', fontSize: '12px',
              fontFamily: 'monospace', lineHeight: 1.6,
              whiteSpace: 'pre-wrap', wordBreak: 'break-word',
              maxHeight: '400px', overflowY: 'auto',
              background: result.ok ? 'var(--color-bg-base)' : 'rgba(239,68,68,0.08)',
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

// ── Pipeline types ─────────────────────────────────────────────────────────────

interface PipelineStepDef {
  id: string
  toolName: string
  params: Record<string, string>
  deps: string[]  // step IDs that must complete before this step
}

interface PipelineStepResult {
  id: string
  name: string
  ok: boolean
  cancelled: boolean  // true = tx cancelled before this step ran
  output: string
  duration_ms: number
}

interface TxDebugEvent {
  session_id: string
  kind: 'prepare' | 'commit' | 'cancel'
  tools: string[]
}

// ── PipelineStepEditor ─────────────────────────────────────────────────────────

interface PipelineStepEditorProps {
  step: PipelineStepDef
  index: number
  allSteps: PipelineStepDef[]
  onChange: (updated: PipelineStepDef) => void
  onDelete: () => void
}

function PipelineStepEditor({ step, index, allSteps, onChange, onDelete }: PipelineStepEditorProps) {
  const tool = TOOLS.find(t => t.name === step.toolName) ?? TOOLS[0]
  const prevSteps = allSteps.slice(0, index)

  const handleToolChange = (toolName: string) => {
    const newTool = TOOLS.find(t => t.name === toolName) ?? TOOLS[0]
    onChange({ ...step, toolName, params: getDefaultParams(newTool) })
  }

  const handleParamChange = (key: string, value: string) => {
    onChange({ ...step, params: { ...step.params, [key]: value } })
  }

  const toggleDep = (depId: string) => {
    const deps = step.deps.includes(depId)
      ? step.deps.filter(d => d !== depId)
      : [...step.deps, depId]
    onChange({ ...step, deps })
  }

  return (
    <div style={{
      border: '1px solid var(--color-border)',
      borderRadius: '8px',
      padding: '10px 12px',
      background: 'var(--color-bg-secondary, var(--color-bg-hover))',
      display: 'flex',
      flexDirection: 'column',
      gap: '8px',
    }}>
      {/* Step header: number + tool selector + delete */}
      <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
        <span style={{
          fontSize: '10px', fontWeight: 700, color: 'var(--color-text-muted)',
          background: 'var(--color-bg-hover)', padding: '2px 6px', borderRadius: '4px',
          flexShrink: 0,
        }}>
          Step {index + 1}
        </span>
        <select
          value={step.toolName}
          onChange={e => handleToolChange(e.target.value)}
          style={{
            flex: 1, fontSize: '12px', padding: '4px 6px',
            borderRadius: '5px', border: '1px solid var(--color-border)',
            background: 'var(--color-bg-base)', color: 'var(--color-text-primary)',
            fontFamily: 'monospace', cursor: 'pointer',
          }}
        >
          {TOOLS.map(t => (
            <option key={t.name} value={t.name}>{t.label}</option>
          ))}
        </select>
        {tool.isWrite && (
          <span style={{
            fontSize: '10px', padding: '1px 5px', borderRadius: '3px',
            background: 'var(--color-warning, #f59e0b)', color: 'white', flexShrink: 0,
          }}>
            寫入
          </span>
        )}
        <button
          onClick={onDelete}
          title="刪除此步驟"
          style={{
            fontSize: '13px', color: 'var(--color-text-muted)',
            padding: '2px 5px', borderRadius: '4px', cursor: 'pointer',
            flexShrink: 0,
          }}
          onMouseEnter={e => (e.currentTarget.style.color = '#ef4444')}
          onMouseLeave={e => (e.currentTarget.style.color = 'var(--color-text-muted)')}
        >
          🗑
        </button>
      </div>

      {/* Description */}
      <div style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>
        {tool.description}
      </div>

      {/* Params */}
      {tool.params.map(param => (
        <div key={param.key} style={{ display: 'flex', flexDirection: 'column', gap: '3px' }}>
          <label style={{ fontSize: '11px', color: 'var(--color-text-secondary)', fontFamily: 'monospace' }}>
            {param.label}
            {param.required && <span style={{ color: 'var(--color-warning, #f59e0b)', marginLeft: '2px' }}>*</span>}
          </label>
          {param.type === 'textarea' ? (
            <textarea
              value={step.params[param.key] ?? ''}
              onChange={e => handleParamChange(param.key, e.target.value)}
              placeholder={param.placeholder}
              rows={3}
              style={{ ...inputStyle, fontSize: '11px', resize: 'vertical' }}
            />
          ) : (
            <input
              type={param.type === 'number' ? 'number' : 'text'}
              value={step.params[param.key] ?? ''}
              onChange={e => handleParamChange(param.key, e.target.value)}
              placeholder={param.placeholder}
              style={{ ...inputStyle, fontSize: '11px', fontFamily: param.type === 'number' ? undefined : 'monospace' }}
            />
          )}
        </div>
      ))}

      {/* Dependency selector (only shown if there are previous steps) */}
      {prevSteps.length > 0 && (
        <div>
          <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginBottom: '5px' }}>
            前置步驟 <span style={{ opacity: 0.7 }}>(需等待完成後才執行)</span>
          </div>
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: '6px' }}>
            {prevSteps.map((ps, pi) => {
              const pTool = TOOLS.find(t => t.name === ps.toolName)
              return (
                <label
                  key={ps.id}
                  style={{
                    display: 'flex', alignItems: 'center', gap: '4px',
                    cursor: 'pointer', fontSize: '11px',
                    color: step.deps.includes(ps.id) ? 'var(--color-accent)' : 'var(--color-text-secondary)',
                    background: step.deps.includes(ps.id) ? 'rgba(99,102,241,0.08)' : 'var(--color-bg-hover)',
                    padding: '2px 7px', borderRadius: '4px',
                    border: `1px solid ${step.deps.includes(ps.id) ? 'var(--color-accent)' : 'var(--color-border)'}`,
                    userSelect: 'none',
                  }}
                >
                  <input
                    type="checkbox"
                    checked={step.deps.includes(ps.id)}
                    onChange={() => toggleDep(ps.id)}
                    style={{ cursor: 'pointer', accentColor: 'var(--color-accent)' }}
                  />
                  Step {pi + 1} · {pTool?.label.replace(/^[^ ]+ /, '') ?? ps.toolName}
                </label>
              )
            })}
          </div>
        </div>
      )}
    </div>
  )
}

// ── PipelineBuilder ────────────────────────────────────────────────────────────

function PipelineBuilder() {
  const [steps, setSteps] = useState<PipelineStepDef[]>([])
  const [running, setRunning] = useState(false)
  const [results, setResults] = useState<PipelineStepResult[] | null>(null)

  const addStep = useCallback(() => {
    const id = `step_${Date.now()}_${Math.random().toString(36).slice(2, 6)}`
    setSteps(prev => [...prev, {
      id,
      toolName: TOOLS[0].name,
      params: getDefaultParams(TOOLS[0]),
      deps: [],
    }])
    setResults(null)
  }, [])

  const updateStep = useCallback((index: number, updated: PipelineStepDef) => {
    setSteps(prev => prev.map((s, i) => i === index ? updated : s))
  }, [])

  const deleteStep = useCallback((index: number, stepId: string) => {
    setSteps(prev => {
      const next = prev.filter((_, i) => i !== index)
      // Remove deleted step from deps of remaining steps
      return next.map(s => ({ ...s, deps: s.deps.filter(d => d !== stepId) }))
    })
    setResults(null)
  }, [])

  const canRun = steps.length > 0 && steps.every(s => {
    const tool = TOOLS.find(t => t.name === s.toolName)
    if (!tool) return false
    return tool.params.filter(p => p.required).every(p => (s.params[p.key] ?? '').trim().length > 0)
  })

  const handleRun = useCallback(async () => {
    setRunning(true)
    setResults(null)
    try {
      const payload = steps.map(s => {
        const tool = TOOLS.find(t => t.name === s.toolName)!
        return {
          id: s.id,
          name: s.toolName,
          args: buildInvokeArgs(tool, s.params),
          deps: s.deps,
        }
      })
      const res = await Promise.race([
        invoke<PipelineStepResult[]>('run_tool_pipeline', { steps: payload }),
        new Promise<never>((_, reject) =>
          setTimeout(() => reject(new Error('Pipeline 逾時（60s），請確認 Vault 已設定')), 60_000)
        ),
      ])
      setResults(res)
    } catch (e) {
      setResults([{ id: '__error__', name: 'Pipeline Error', ok: false, cancelled: false, output: String(e), duration_ms: 0 }])
    } finally {
      setRunning(false)
    }
  }, [steps])

  // Build a result map for quick lookup
  const resultMap = results
    ? Object.fromEntries(results.map(r => [r.id, r]))
    : null

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '10px' }}>
      {/* Empty state */}
      {steps.length === 0 && (
        <div style={{
          textAlign: 'center', padding: '32px 16px',
          color: 'var(--color-text-muted)', fontSize: '13px',
          border: '1px dashed var(--color-border)', borderRadius: '8px',
        }}>
          <div style={{ fontSize: '24px', marginBottom: '8px' }}>🔗</div>
          點擊「＋ 新增步驟」開始建立工具流程
          <div style={{ fontSize: '11px', marginTop: '6px', opacity: 0.7 }}>
            可選擇多個工具，設定前置步驟，組合成可重現的 Pipeline
          </div>
        </div>
      )}

      {/* Step editors */}
      {steps.map((step, index) => {
        const isParallel = index > 0 && step.deps.length === 0
        const depLabels = step.deps
          .map(d => steps.findIndex(s => s.id === d))
          .filter(i => i >= 0)
          .map(i => `Step ${i + 1}`)
        return (
          <div key={step.id}>
            {/* Connector between steps */}
            {index > 0 && (
              <div style={{ textAlign: 'center', margin: '2px 0', fontSize: '11px', color: 'var(--color-text-muted)' }}>
                {isParallel
                  ? <span title="無前置步驟，與前一步並行">⇒ 並行</span>
                  : <span>↓ 等待 {depLabels.join(', ')}</span>
                }
              </div>
            )}
            <PipelineStepEditor
              step={step}
              index={index}
              allSteps={steps}
              onChange={updated => updateStep(index, updated)}
              onDelete={() => deleteStep(index, step.id)}
            />
            {/* Inline result (shown after execution) */}
            {resultMap && resultMap[step.id] && (() => {
              const r = resultMap[step.id]
              if (r.cancelled) return (
                <div style={{
                  marginTop: '4px', padding: '5px 10px', borderRadius: '6px',
                  fontSize: '11px', fontFamily: 'monospace',
                  background: 'var(--color-bg-hover)',
                  border: '1px solid var(--color-border)',
                  color: 'var(--color-text-muted)',
                }}>
                  ⏸ 已取消（未執行）
                </div>
              )
              return (
                <div style={{
                  marginTop: '4px', padding: '6px 10px', borderRadius: '6px',
                  fontSize: '11px', fontFamily: 'monospace', lineHeight: 1.5,
                  whiteSpace: 'pre-wrap', wordBreak: 'break-word',
                  maxHeight: '300px', overflowY: 'auto',
                  background: r.ok ? 'var(--color-bg-base)' : 'rgba(239,68,68,0.08)',
                  border: `1px solid ${r.ok ? 'var(--color-border)' : 'rgba(239,68,68,0.3)'}`,
                  color: r.ok ? 'var(--color-text-primary)' : '#ef4444',
                }}>
                  <span style={{ fontWeight: 700, color: r.ok ? 'var(--color-success, #22c55e)' : '#ef4444' }}>
                    {r.ok ? '✅' : '❌'}
                  </span>
                  <span style={{ color: 'var(--color-text-muted)', marginLeft: '4px' }}>
                    {r.duration_ms}ms
                  </span>
                  {'\n'}
                  {r.output}
                </div>
              )
            })()}
          </div>
        )
      })}

      {/* Action buttons */}
      <div style={{ display: 'flex', gap: '8px', alignItems: 'center', flexWrap: 'wrap' }}>
        <button
          onClick={addStep}
          style={{
            padding: '6px 12px', borderRadius: '6px', fontSize: '12px',
            border: '1px dashed var(--color-border)', cursor: 'pointer',
            color: 'var(--color-text-secondary)', background: 'transparent',
          }}
          onMouseEnter={e => (e.currentTarget.style.borderColor = 'var(--color-accent)')}
          onMouseLeave={e => (e.currentTarget.style.borderColor = 'var(--color-border)')}
        >
          ＋ 新增步驟
        </button>
        <button
          onClick={handleRun}
          disabled={!canRun || running}
          style={{
            padding: '6px 14px', borderRadius: '6px', fontSize: '12px',
            background: canRun && !running ? 'var(--color-accent)' : 'var(--color-border)',
            color: canRun && !running ? 'white' : 'var(--color-text-muted)',
            cursor: canRun && !running ? 'pointer' : 'not-allowed',
            fontWeight: 600,
          }}
        >
          {running ? '執行中…' : `▶ 執行 Pipeline${steps.length > 0 ? ` (${steps.length} 步)` : ''}`}
        </button>
        {steps.length > 0 && !running && (
          <button
            onClick={() => { setSteps([]); setResults(null) }}
            style={{
              padding: '6px 10px', borderRadius: '6px', fontSize: '12px',
              color: 'var(--color-text-muted)', background: 'transparent',
              cursor: 'pointer',
            }}
            onMouseEnter={e => (e.currentTarget.style.color = 'var(--color-text-primary)')}
            onMouseLeave={e => (e.currentTarget.style.color = 'var(--color-text-muted)')}
          >
            清除全部
          </button>
        )}
      </div>

      {/* Global error (for __error__ id) */}
      {results && results.some(r => r.id === '__error__') && (
        <div style={{
          padding: '8px 10px', borderRadius: '6px', fontSize: '12px',
          background: 'rgba(239,68,68,0.08)', border: '1px solid rgba(239,68,68,0.3)',
          color: '#ef4444', fontFamily: 'monospace',
        }}>
          {results.find(r => r.id === '__error__')!.output}
        </div>
      )}
    </div>
  )
}

// ── Shared inner content ───────────────────────────────────────────────────────

function AgentToolInner({ onClose }: { onClose?: () => void }) {
  const [mode, setMode] = useState<'tools' | 'pipeline'>('tools')
  const [txState, setTxState] = useState<TxDebugEvent | null>(null)
  const txTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    let unlisten: (() => void) | undefined
    listen<TxDebugEvent>('agent:tx_debug', (e) => {
      setTxState(e.payload)
      if (e.payload.kind !== 'prepare') {
        if (txTimerRef.current) clearTimeout(txTimerRef.current)
        txTimerRef.current = setTimeout(() => setTxState(null), 2500)
      }
    }).then(u => { unlisten = u })
    return () => {
      unlisten?.()
      if (txTimerRef.current) clearTimeout(txTimerRef.current)
    }
  }, [])

  const handleCancel = useCallback(() => {
    invoke('cancel_tool_test').catch(() => {})
  }, [])

  const isPreparing = txState?.kind === 'prepare'

  const txBanner = txState ? (() => {
    switch (txState.kind) {
      case 'prepare': return {
        bg: 'rgba(234,179,8,0.12)', border: 'rgba(234,179,8,0.3)',
        color: '#ca8a04', icon: '⏳', label: `執行中 · ${txState.tools.join(', ')}`,
      }
      case 'commit': return {
        bg: 'rgba(34,197,94,0.1)', border: 'rgba(34,197,94,0.3)',
        color: 'var(--color-success, #22c55e)', icon: '✅', label: '已提交',
      }
      case 'cancel': return {
        bg: 'rgba(239,68,68,0.08)', border: 'rgba(239,68,68,0.3)',
        color: '#ef4444', icon: '🚫', label: '已取消',
      }
    }
  })() : null

  return (
    <div style={{ display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0, overflow: 'hidden' }}>
      {/* Header */}
      <div style={{
        display: 'flex', alignItems: 'center', justifyContent: 'space-between',
        padding: '14px 16px', borderBottom: '1px solid var(--color-border)',
        flexShrink: 0, gap: '10px',
      }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
          <span style={{ fontSize: '14px', fontWeight: 700, color: 'var(--color-text-primary)' }}>
            ⚡ Agent Tool 測試台
          </span>
          <div style={{
            display: 'flex', gap: '2px',
            background: 'var(--color-bg-hover)',
            borderRadius: '6px', padding: '2px',
          }}>
            {(['tools', 'pipeline'] as const).map(m => (
              <button
                key={m}
                onClick={() => setMode(m)}
                style={{
                  padding: '3px 10px', borderRadius: '4px', fontSize: '11px',
                  fontWeight: 600, cursor: 'pointer',
                  background: mode === m ? 'var(--color-bg-base)' : 'transparent',
                  color: mode === m ? 'var(--color-text-primary)' : 'var(--color-text-muted)',
                  boxShadow: mode === m ? '0 1px 3px rgba(0,0,0,0.12)' : 'none',
                }}
              >
                {m === 'tools' ? '個別測試' : '🔗 Pipeline'}
              </button>
            ))}
          </div>
          <span style={{ fontSize: '12px', color: 'var(--color-text-muted)' }}>
            {mode === 'tools' ? `${TOOLS.length} 個工具` : '自訂執行流程'}
          </span>
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
          {isPreparing && (
            <button
              onClick={handleCancel}
              title="中止目前執行"
              style={{
                padding: '4px 10px', borderRadius: '5px', fontSize: '11px',
                fontWeight: 600, cursor: 'pointer',
                background: 'rgba(239,68,68,0.1)', color: '#ef4444',
                border: '1px solid rgba(239,68,68,0.3)',
              }}
            >
              ⏹ 中止
            </button>
          )}
          {onClose && (
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
          )}
        </div>
      </div>

      {/* TX status banner */}
      {txBanner && (
        <div style={{
          padding: '6px 14px', fontSize: '11px', fontWeight: 600,
          display: 'flex', alignItems: 'center', gap: '6px',
          background: txBanner.bg,
          borderBottom: `1px solid ${txBanner.border}`,
          color: txBanner.color,
          flexShrink: 0,
        }}>
          <span>{txBanner.icon}</span>
          <span>Transaction · {txBanner.label}</span>
          {isPreparing && (
            <span style={{ marginLeft: 'auto', opacity: 0.7, fontWeight: 400 }}>
              session {txState!.session_id.slice(0, 8)}
            </span>
          )}
        </div>
      )}

      {/* Scroll area — block container so children grow to natural height */}
      <div style={{ flex: 1, minHeight: 0, overflowY: 'auto', padding: '12px' }}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: '8px' }}>
          {mode === 'tools'
            ? TOOLS.map(tool => <ToolCard key={tool.name} tool={tool} />)
            : <PipelineBuilder />
          }
        </div>
      </div>
    </div>
  )
}

// ── AgentToolContent (tab version, fills editor area) ─────────────────────────

export function AgentToolContent() {
  return <AgentToolInner />
}

// ── AgentToolPanel (modal overlay) ────────────────────────────────────────────

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
    >
      <div style={{
        width: '620px',
        maxWidth: '96vw',
        maxHeight: '94vh',
        background: 'var(--color-bg-base)',
        borderRadius: '12px',
        border: '1px solid var(--color-border)',
        display: 'flex', flexDirection: 'column',
        overflow: 'hidden',
        boxShadow: '0 20px 60px rgba(0,0,0,0.3)',
      }}>
        <AgentToolInner onClose={onClose} />
      </div>
    </div>
  )
}
