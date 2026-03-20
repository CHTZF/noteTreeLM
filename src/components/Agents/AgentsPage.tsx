import { useState, useEffect, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import type { AgentSkill } from '../../types/models'

// ── Types ─────────────────────────────────────────────────────────────────────

interface AgentDefinition {
  def_id: string
  vault_id: string
  name: string
  description: string
  kind: 'main' | 'sub'
  skill_ids: string[]
  tool_names: string[]
  system_prompt: string
  max_rounds: number
  is_active: boolean
  is_builtin: boolean
  trigger: string
  created_at: number
  status: 'active' | 'sleep'
  slept_at: number | null
  use_count: number
  last_used_at: number | null
}

interface EphemeralAgent {
  sub_session_id: string
  def_id: string
  def_name: string
  task: string
  status: 'running' | 'done' | 'error'
  result_preview: string
  started_at: number
  ended_at: number | null
}

// ── Constants ─────────────────────────────────────────────────────────────────

const ALL_TOOLS = [
  'search_vault', 'read_note', 'open_note', 'list_structure', 'query_memory',
  'create_note', 'update_note', 'create_folder',
  'call_external_ai', 'list_recent_conversations', 'create_agent_skill',
]

// ── AgentsPage ────────────────────────────────────────────────────────────────

export default function AgentsPage() {
  const [defs, setDefs] = useState<AgentDefinition[]>([])
  const [skills, setSkills] = useState<AgentSkill[]>([])
  const [ephemeral, setEphemeral] = useState<EphemeralAgent[]>([])
  const [loading, setLoading] = useState(true)
  const [showCreate, setShowCreate] = useState(false)

  const loadDefs = useCallback(() => {
    invoke<AgentDefinition[]>('list_agent_definitions')
      .then(setDefs)
      .catch(() => {})
      .finally(() => setLoading(false))
  }, [])

  const loadSkills = useCallback(() => {
    invoke<AgentSkill[]>('list_agent_skills', { activeOnly: false })
      .then(setSkills)
      .catch(() => {})
  }, [])

  const loadEphemeral = useCallback(() => {
    invoke<EphemeralAgent[]>('list_ephemeral_agents')
      .then(setEphemeral)
      .catch(() => {})
  }, [])

  useEffect(() => {
    loadDefs()
    loadSkills()
    loadEphemeral()
  }, [loadDefs, loadSkills, loadEphemeral])

  // 監聽 ephemeral 更新事件
  useEffect(() => {
    const unlisten = listen('system_agent:ephemeral_updated', () => {
      loadEphemeral()
    })
    return () => { unlisten.then(fn => fn()) }
  }, [loadEphemeral])

  const handleToggle = async (def_id: string, is_active: boolean) => {
    await invoke('toggle_agent_definition', { defId: def_id, isActive: is_active })
    setDefs(prev => prev.map(d => d.def_id === def_id ? { ...d, is_active } : d))
  }

  const handleDelete = async (def_id: string) => {
    await invoke('delete_agent_definition', { defId: def_id })
    setDefs(prev => prev.filter(d => d.def_id !== def_id))
  }

  const handleWake = async (def_id: string) => {
    await invoke('wake_agent_definition', { defId: def_id })
    setDefs(prev => prev.map(d => d.def_id === def_id ? { ...d, status: 'active' as const, slept_at: null } : d))
  }

  const handleClearEphemeral = async () => {
    await invoke('clear_ephemeral_agents')
    setEphemeral([])
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', overflow: 'hidden', background: 'var(--color-bg-base)' }}>
      <div style={{ padding: '10px 14px 6px', borderBottom: '1px solid var(--color-border)', flexShrink: 0 }}>
        <span style={{ fontSize: 13, fontWeight: 600, color: 'var(--color-text-secondary)', letterSpacing: '0.05em' }}>
          Agents
        </span>
      </div>

      <div style={{ flex: 1, overflowY: 'auto', padding: '12px 14px' }}>

        {/* ── 已定義的 Agents ── */}
        <div style={{ marginBottom: 20 }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
            <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--color-text-muted)', textTransform: 'uppercase', letterSpacing: '0.06em' }}>
              Agent 定義
            </span>
            <button
              style={{ fontSize: 11, padding: '2px 8px', borderRadius: 4, background: 'var(--color-accent)', color: '#fff', border: 'none', cursor: 'pointer' }}
              onClick={() => setShowCreate(v => !v)}
            >
              + 新增
            </button>
          </div>

          {showCreate && (
            <CreateDefForm
              skills={skills}
              onCreated={(def) => { setDefs(prev => [...prev, def]); setShowCreate(false) }}
              onCancel={() => setShowCreate(false)}
            />
          )}

          {loading && <div style={{ color: 'var(--color-text-muted)', fontSize: 12 }}>載入中…</div>}
          {!loading && defs.filter(d => d.status !== 'sleep').map(def => (
            <DefCard
              key={def.def_id}
              def={def}
              skills={skills}
              onToggle={handleToggle}
              onDelete={handleDelete}
              onUpdated={(updated) => setDefs(prev => prev.map(d => d.def_id === updated.def_id ? updated : d))}
            />
          ))}
        </div>

        {/* ── 休眠中的 Agents ── */}
        {!loading && defs.some(d => d.status === 'sleep') && (
          <div style={{ marginBottom: 20 }}>
            <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--color-text-muted)', textTransform: 'uppercase', letterSpacing: '0.06em' }}>
              休眠中
            </span>
            {defs.filter(d => d.status === 'sleep').map(def => (
              <div key={def.def_id} style={{
                border: '1px solid var(--color-border)',
                borderRadius: 6, marginTop: 8, padding: '8px 12px',
                opacity: 0.55, background: 'var(--color-bg-surface, var(--color-bg-overlay))',
                display: 'flex', alignItems: 'center', gap: 8,
              }}>
                <span style={{ fontSize: 13, fontWeight: 600, color: 'var(--color-text-primary)', flex: 1 }}>{def.name}</span>
                {def.slept_at && (
                  <span style={{ fontSize: 11, color: 'var(--color-text-muted)' }}>
                    休眠於 {new Date(def.slept_at).toLocaleDateString()}
                  </span>
                )}
                <button
                  onClick={() => handleWake(def.def_id)}
                  style={{ fontSize: 11, padding: '2px 8px', borderRadius: 4, background: 'rgba(99,102,241,0.15)', border: '1px solid var(--color-accent)', color: 'var(--color-accent)', cursor: 'pointer' }}
                >
                  喚醒
                </button>
                {!def.is_builtin && (
                  <button onClick={() => handleDelete(def.def_id)} style={{ fontSize: 11, padding: '2px 6px', borderRadius: 4, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', color: 'var(--color-error, #ef4444)', cursor: 'pointer' }}>刪除</button>
                )}
              </div>
            ))}
          </div>
        )}

        {/* ── 本次 Session 臨時 Agents ── */}
        <div>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
            <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--color-text-muted)', textTransform: 'uppercase', letterSpacing: '0.06em' }}>
              本次對話 ({ephemeral.length})
            </span>
            {ephemeral.length > 0 && (
              <button
                style={{ fontSize: 11, padding: '2px 8px', borderRadius: 4, background: 'var(--color-bg-overlay)', color: 'var(--color-text-muted)', border: '1px solid var(--color-border)', cursor: 'pointer' }}
                onClick={handleClearEphemeral}
              >
                清除
              </button>
            )}
          </div>
          {ephemeral.length === 0 && (
            <div style={{ fontSize: 12, color: 'var(--color-text-muted)', fontStyle: 'italic' }}>
              本次對話尚無動態建立的 agent
            </div>
          )}
          {ephemeral.map(e => (
            <EphemeralCard key={e.sub_session_id} agent={e} />
          ))}
        </div>
      </div>
    </div>
  )
}

// ── DefCard ───────────────────────────────────────────────────────────────────

function DefCard({
  def, skills, onToggle, onDelete, onUpdated,
}: {
  def: AgentDefinition
  skills: AgentSkill[]
  onToggle: (id: string, active: boolean) => void
  onDelete: (id: string) => void
  onUpdated: (updated: AgentDefinition) => void
}) {
  const [editing, setEditing] = useState(false)
  const [editName, setEditName] = useState(def.name)
  const [editDesc, setEditDesc] = useState(def.description)
  const [editTools, setEditTools] = useState<string[]>(def.tool_names)
  const [editSkills, setEditSkills] = useState<string[]>(def.skill_ids)
  const [editPrompt, setEditPrompt] = useState(def.system_prompt)
  const [editRounds, setEditRounds] = useState(def.max_rounds)
  const [editTrigger, setEditTrigger] = useState(def.trigger ?? '')
  const [saving, setSaving] = useState(false)

  const skillMap = Object.fromEntries(skills.map(s => [s.skill_id, s.title]))

  const handleSave = async () => {
    setSaving(true)
    try {
      await invoke('update_agent_definition', {
        defId: def.def_id,
        name: editName,
        description: editDesc,
        skillIds: editSkills,
        toolNames: editTools,
        systemPrompt: editPrompt || null,
        maxRounds: editRounds,
        trigger: editTrigger || null,
      })
      onUpdated({ ...def, name: editName, description: editDesc, skill_ids: editSkills, tool_names: editTools, system_prompt: editPrompt, max_rounds: editRounds, trigger: editTrigger })
      setEditing(false)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div style={{
      border: `1px solid ${def.is_active ? 'var(--color-border)' : 'var(--color-border-muted, var(--color-border))'}`,
      borderRadius: 6, marginBottom: 8, padding: '10px 12px',
      opacity: def.is_active ? 1 : 0.6,
      background: 'var(--color-bg-surface, var(--color-bg-overlay))',
    }}>
      {/* Header */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: editing ? 8 : 4 }}>
        {editing
          ? <input value={editName} onChange={e => setEditName(e.target.value)} style={{ flex: 1, fontSize: 13, fontWeight: 600, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '2px 6px', color: 'var(--color-text-primary)' }} />
          : <span style={{ flex: 1, fontSize: 13, fontWeight: 600, color: 'var(--color-text-primary)' }}>{def.name}</span>
        }
        {def.is_builtin && (
          <span style={{ fontSize: 10, padding: '1px 5px', borderRadius: 4, background: 'rgba(99,102,241,0.15)', color: 'var(--color-accent)', border: '1px solid var(--color-accent)' }}>內建</span>
        )}
        <span style={{ fontSize: 10, padding: '1px 5px', borderRadius: 4, background: 'var(--color-bg-overlay)', color: 'var(--color-text-muted)', border: '1px solid var(--color-border)' }}>
          {def.kind === 'main' ? '主 Agent' : 'Sub-Agent'}
        </span>
        {!editing && (
          <>
            <button onClick={() => setEditing(true)} style={{ fontSize: 11, padding: '2px 6px', borderRadius: 4, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', color: 'var(--color-text-muted)', cursor: 'pointer' }}>編輯</button>
            <button
              onClick={() => onToggle(def.def_id, !def.is_active)}
              style={{ fontSize: 11, padding: '2px 6px', borderRadius: 4, background: def.is_active ? 'rgba(34,197,94,0.15)' : 'var(--color-bg-overlay)', border: `1px solid ${def.is_active ? '#22c55e' : 'var(--color-border)'}`, color: def.is_active ? '#22c55e' : 'var(--color-text-muted)', cursor: 'pointer' }}
            >
              {def.is_active ? '啟用中' : '已停用'}
            </button>
            {!def.is_builtin && (
              <button onClick={() => onDelete(def.def_id)} style={{ fontSize: 11, padding: '2px 6px', borderRadius: 4, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', color: 'var(--color-error, #ef4444)', cursor: 'pointer' }}>刪除</button>
            )}
          </>
        )}
      </div>

      {editing ? (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
          <textarea value={editDesc} onChange={e => setEditDesc(e.target.value)} rows={2} placeholder="描述" style={{ fontSize: 12, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '4px 6px', color: 'var(--color-text-primary)', resize: 'vertical' }} />

          <div>
            <div style={{ fontSize: 11, color: 'var(--color-text-muted)', marginBottom: 4 }}>工具</div>
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4 }}>
              {ALL_TOOLS.map(tool => (
                <label key={tool} style={{ display: 'flex', alignItems: 'center', gap: 3, fontSize: 11, cursor: 'pointer' }}>
                  <input type="checkbox" checked={editTools.includes(tool)} onChange={e => setEditTools(prev => e.target.checked ? [...prev, tool] : prev.filter(t => t !== tool))} />
                  {tool}
                </label>
              ))}
            </div>
          </div>

          <div>
            <div style={{ fontSize: 11, color: 'var(--color-text-muted)', marginBottom: 4 }}>技能規範</div>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 3, maxHeight: 120, overflowY: 'auto' }}>
              {skills.map(s => (
                <label key={s.skill_id} style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 11, cursor: 'pointer' }}>
                  <input type="checkbox" checked={editSkills.includes(s.skill_id)} onChange={e => setEditSkills(prev => e.target.checked ? [...prev, s.skill_id] : prev.filter(id => id !== s.skill_id))} />
                  {s.title}
                  <span style={{ color: 'var(--color-text-muted)', fontSize: 10 }}>({s.injection_mode === 'active' ? '主動' : '被動'})</span>
                </label>
              ))}
              {skills.length === 0 && <span style={{ fontSize: 11, color: 'var(--color-text-muted)', fontStyle: 'italic' }}>尚無技能規範</span>}
            </div>
          </div>

          <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <span style={{ fontSize: 11, color: 'var(--color-text-muted)' }}>最大輪數</span>
            <input type="number" value={editRounds} min={1} max={20} onChange={e => setEditRounds(Number(e.target.value))} style={{ width: 50, fontSize: 12, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '2px 4px', color: 'var(--color-text-primary)' }} />
          </div>

          <div>
            <div style={{ fontSize: 11, color: 'var(--color-text-muted)', marginBottom: 4 }}>觸發詞（Pre-routing embedding 比對用）</div>
            <input value={editTrigger} onChange={e => setEditTrigger(e.target.value)} placeholder="例如：搜尋 找資料 查詢知識庫" style={{ width: '100%', fontSize: 11, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '4px 6px', color: 'var(--color-text-primary)' }} />
            <div style={{ fontSize: 10, color: 'var(--color-text-muted)', marginTop: 2 }}>設定後，當使用者輸入與此觸發詞相似度 &gt; 0.75 時，系統將直接路由到此 agent，跳過 Chat LLM</div>
          </div>

          <textarea value={editPrompt} onChange={e => setEditPrompt(e.target.value)} rows={3} placeholder="自訂 system prompt（留空使用預設）" style={{ fontSize: 11, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '4px 6px', color: 'var(--color-text-primary)', resize: 'vertical' }} />

          <div style={{ display: 'flex', gap: 6 }}>
            <button onClick={handleSave} disabled={saving} style={{ fontSize: 12, padding: '3px 10px', borderRadius: 4, background: 'var(--color-accent)', color: '#fff', border: 'none', cursor: 'pointer' }}>
              {saving ? '儲存中…' : '儲存'}
            </button>
            <button onClick={() => { setEditing(false); setEditName(def.name); setEditDesc(def.description); setEditTools(def.tool_names); setEditSkills(def.skill_ids); setEditPrompt(def.system_prompt); setEditRounds(def.max_rounds); setEditTrigger(def.trigger ?? '') }} style={{ fontSize: 12, padding: '3px 10px', borderRadius: 4, background: 'var(--color-bg-overlay)', color: 'var(--color-text-muted)', border: '1px solid var(--color-border)', cursor: 'pointer' }}>
              取消
            </button>
          </div>
        </div>
      ) : (
        <>
          {def.description && <div style={{ fontSize: 11, color: 'var(--color-text-secondary)', marginBottom: 4 }}>{def.description}</div>}
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4, marginTop: 4 }}>
            {def.tool_names.map(t => (
              <span key={t} style={{ fontSize: 10, padding: '1px 5px', borderRadius: 4, background: 'var(--color-bg-overlay)', color: 'var(--color-text-muted)', border: '1px solid var(--color-border)' }}>{t}</span>
            ))}
          </div>
          {def.skill_ids.length > 0 && (
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4, marginTop: 4 }}>
              {def.skill_ids.map(sid => (
                <span key={sid} style={{ fontSize: 10, padding: '1px 5px', borderRadius: 4, background: 'rgba(99,102,241,0.1)', color: 'var(--color-accent)', border: '1px solid rgba(99,102,241,0.3)' }}>
                  {skillMap[sid] ?? sid}
                </span>
              ))}
            </div>
          )}
          <div style={{ fontSize: 10, color: 'var(--color-text-muted)', marginTop: 4 }}>
            最多 {def.max_rounds} 輪
            {def.trigger && (
              <span style={{ marginLeft: 8, padding: '0 4px', borderRadius: 3, background: 'rgba(245,158,11,0.15)', color: '#d97706', border: '1px solid rgba(245,158,11,0.3)' }}>
                ⚡ pre-route
              </span>
            )}
          </div>
        </>
      )}
    </div>
  )
}

// ── CreateDefForm ─────────────────────────────────────────────────────────────

function CreateDefForm({ skills, onCreated, onCancel }: { skills: AgentSkill[], onCreated: (def: AgentDefinition) => void, onCancel: () => void }) {
  const [name, setName] = useState('')
  const [desc, setDesc] = useState('')
  const [tools, setTools] = useState<string[]>([])
  const [skillIds, setSkillIds] = useState<string[]>([])
  const [maxRounds, setMaxRounds] = useState(5)
  const [trigger, setTrigger] = useState('')
  const [creating, setCreating] = useState(false)

  const handleCreate = async () => {
    if (!name.trim()) return
    setCreating(true)
    try {
      const def = await invoke<AgentDefinition>('save_agent_definition', {
        name: name.trim(), description: desc.trim(),
        kind: 'sub', skillIds, toolNames: tools, maxRounds,
        trigger: trigger.trim() || null,
      })
      onCreated(def)
    } finally {
      setCreating(false)
    }
  }

  return (
    <div style={{ border: '1px solid var(--color-accent)', borderRadius: 6, padding: '10px 12px', marginBottom: 10, background: 'var(--color-bg-surface, var(--color-bg-overlay))' }}>
      <input value={name} onChange={e => setName(e.target.value)} placeholder="Agent 名稱" style={{ width: '100%', fontSize: 13, fontWeight: 600, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '4px 8px', color: 'var(--color-text-primary)', marginBottom: 6 }} />
      <textarea value={desc} onChange={e => setDesc(e.target.value)} rows={2} placeholder="描述（選填）" style={{ width: '100%', fontSize: 12, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '4px 6px', color: 'var(--color-text-primary)', resize: 'vertical', marginBottom: 6 }} />

      <div style={{ fontSize: 11, color: 'var(--color-text-muted)', marginBottom: 4 }}>工具</div>
      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4, marginBottom: 8 }}>
        {ALL_TOOLS.map(tool => (
          <label key={tool} style={{ display: 'flex', alignItems: 'center', gap: 3, fontSize: 11, cursor: 'pointer' }}>
            <input type="checkbox" checked={tools.includes(tool)} onChange={e => setTools(prev => e.target.checked ? [...prev, tool] : prev.filter(t => t !== tool))} />
            {tool}
          </label>
        ))}
      </div>

      {skills.length > 0 && (
        <>
          <div style={{ fontSize: 11, color: 'var(--color-text-muted)', marginBottom: 4 }}>技能規範</div>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 3, maxHeight: 100, overflowY: 'auto', marginBottom: 8 }}>
            {skills.map(s => (
              <label key={s.skill_id} style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 11, cursor: 'pointer' }}>
                <input type="checkbox" checked={skillIds.includes(s.skill_id)} onChange={e => setSkillIds(prev => e.target.checked ? [...prev, s.skill_id] : prev.filter(id => id !== s.skill_id))} />
                {s.title}
              </label>
            ))}
          </div>
        </>
      )}

      <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 8 }}>
        <span style={{ fontSize: 11, color: 'var(--color-text-muted)' }}>最大輪數</span>
        <input type="number" value={maxRounds} min={1} max={20} onChange={e => setMaxRounds(Number(e.target.value))} style={{ width: 50, fontSize: 12, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '2px 4px', color: 'var(--color-text-primary)' }} />
      </div>

      <div style={{ marginBottom: 8 }}>
        <div style={{ fontSize: 11, color: 'var(--color-text-muted)', marginBottom: 4 }}>觸發詞（選填）</div>
        <input value={trigger} onChange={e => setTrigger(e.target.value)} placeholder="例如：搜尋 找資料 查詢知識庫" style={{ width: '100%', fontSize: 11, background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', borderRadius: 4, padding: '4px 6px', color: 'var(--color-text-primary)' }} />
      </div>

      <div style={{ display: 'flex', gap: 6 }}>
        <button onClick={handleCreate} disabled={creating || !name.trim()} style={{ fontSize: 12, padding: '3px 10px', borderRadius: 4, background: 'var(--color-accent)', color: '#fff', border: 'none', cursor: 'pointer' }}>
          {creating ? '建立中…' : '建立'}
        </button>
        <button onClick={onCancel} style={{ fontSize: 12, padding: '3px 10px', borderRadius: 4, background: 'var(--color-bg-overlay)', color: 'var(--color-text-muted)', border: '1px solid var(--color-border)', cursor: 'pointer' }}>
          取消
        </button>
      </div>
    </div>
  )
}

// ── EphemeralCard ─────────────────────────────────────────────────────────────

function EphemeralCard({ agent }: { agent: EphemeralAgent }) {
  const statusColor = { running: '#f59e0b', done: '#22c55e', error: '#ef4444' }[agent.status]
  const elapsed = agent.ended_at ? Math.round((agent.ended_at - agent.started_at) / 1000) : null

  return (
    <div style={{ border: '1px solid var(--color-border)', borderRadius: 6, marginBottom: 6, padding: '8px 10px', background: 'var(--color-bg-surface, var(--color-bg-overlay))' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 3 }}>
        <span style={{ width: 8, height: 8, borderRadius: '50%', background: statusColor, flexShrink: 0, display: 'inline-block' }} />
        <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--color-text-primary)' }}>{agent.def_name}</span>
        <span style={{ fontSize: 10, color: 'var(--color-text-muted)', marginLeft: 'auto' }}>
          {agent.status === 'running' ? '執行中…' : elapsed !== null ? `${elapsed}s` : ''}
        </span>
      </div>
      <div style={{ fontSize: 11, color: 'var(--color-text-secondary)', marginBottom: 2 }}>
        <strong>任務：</strong>{agent.task.slice(0, 80)}{agent.task.length > 80 ? '…' : ''}
      </div>
      {agent.result_preview && (
        <div style={{ fontSize: 11, color: 'var(--color-text-muted)', fontStyle: 'italic' }}>
          {agent.result_preview.slice(0, 100)}{agent.result_preview.length > 100 ? '…' : ''}
        </div>
      )}
    </div>
  )
}
