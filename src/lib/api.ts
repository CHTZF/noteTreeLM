const BASE = 'http://127.0.0.1:7787/api/v1'

// Auth token management
let _token: string | null = localStorage.getItem('daemon_token')

export function setToken(t: string | null) {
  _token = t
  if (t) {
    localStorage.setItem('daemon_token', t)
  } else {
    localStorage.removeItem('daemon_token')
  }
}

export function getToken() {
  return _token
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { 'Content-Type': 'application/json' }
  if (_token) headers['Authorization'] = `Bearer ${_token}`
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers,
    body: body ? JSON.stringify(body) : undefined,
  })
  if (!res.ok) {
    const err = await res.text()
    throw new Error(err || `HTTP ${res.status}`)
  }
  return res.json()
}

export const api = {
  // Auth
  login: (username: string, password: string) =>
    request<{ token: string; username: string; expires_at: number }>('POST', '/auth/login', { username, password }),
  logout: () => request<void>('POST', '/auth/logout'),
  session: () => request<{ username: string; token: string; expires_at: number } | null>('GET', '/auth/session'),
  register: (username: string, password: string) =>
    request<{ ok: boolean; username: string }>('POST', '/auth/register', { username, password }),

  // Settings
  getSettings: () => request<Record<string, string>>('GET', '/settings'),
  saveSettings: (data: Record<string, unknown>) => request<{ ok: boolean }>('POST', '/settings', data),
  getUserSettings: () => request<Record<string, string>>('GET', '/settings/user'),
  saveUserSettings: (data: Record<string, unknown>) => request<{ ok: boolean }>('POST', '/settings/user', data),

  // Conversations
  listConversations: (vaultId?: string, mode?: string) =>
    request<unknown[]>('GET', `/conversations${vaultId ? `?vault_id=${encodeURIComponent(vaultId)}&mode=${encodeURIComponent(mode ?? 'chat')}` : ''}`),
  createConversation: (data: unknown) => request<{ id: string }>('POST', '/conversations', data),
  getConversation: (id: string) => request<unknown>('GET', `/conversations/${encodeURIComponent(id)}`),
  deleteConversation: (id: string) => request<{ ok: boolean }>('DELETE', `/conversations/${encodeURIComponent(id)}`),
  updateConversationTitle: (id: string, title: string) =>
    request<{ ok: boolean }>('PATCH', `/conversations/${encodeURIComponent(id)}/title`, { title }),
  saveConversationMessages: (id: string, messages: unknown[]) =>
    request<{ ok: boolean }>('PUT', `/conversations/${encodeURIComponent(id)}/messages`, { messages }),

  // Vaults
  listVaults: () => request<unknown[]>('GET', '/vaults'),
  registerVault: (path: string, account: string) =>
    request<{ vault_id: string }>('POST', '/vaults', { path, account }),
  getVaultStructure: (vaultId: string) =>
    request<unknown>('GET', `/vaults/${encodeURIComponent(vaultId)}/structure`),
  scanVault: (vaultId: string) =>
    request<{ ok: boolean; indexed: number }>('POST', `/vaults/${encodeURIComponent(vaultId)}/scan`),

  // Notes
  listNotes: (vaultId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/notes`),
  readNote: (vaultId: string, path: string) =>
    request<{ content: string; path: string }>('GET', `/vaults/${encodeURIComponent(vaultId)}/notes/read?path=${encodeURIComponent(path)}`),
  createNote: (vaultId: string, data: unknown) =>
    request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/notes`, data),
  updateNote: (vaultId: string, data: unknown) =>
    request<{ ok: boolean }>('PUT', `/vaults/${encodeURIComponent(vaultId)}/notes`, data),
  deleteNote: (vaultId: string, path: string) =>
    request<{ ok: boolean }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/notes?path=${encodeURIComponent(path)}`),

  // Search
  search: (vaultId: string, query: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/search?q=${encodeURIComponent(query)}`),

  // Knowledge base
  listImportSessions: (vaultId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/kb/sessions`),
  createImportSession: (vaultId: string, data: unknown) =>
    request<{ session_id: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/kb/sessions`, data),
  getImportSession: (vaultId: string, sessionId: string) =>
    request<unknown>('GET', `/vaults/${encodeURIComponent(vaultId)}/kb/sessions/${encodeURIComponent(sessionId)}`),
  listImportPages: (vaultId: string, sessionId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/kb/sessions/${encodeURIComponent(sessionId)}/pages`),
  listKnowledgeItems: (vaultId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/kb/items`),
  createKnowledgeItem: (vaultId: string, data: unknown) =>
    request<{ item_id: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/kb/items`, data),

  // Agents
  listAgentDefinitions: (vaultId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/agents`),
  createAgentDefinition: (vaultId: string, data: unknown) =>
    request<{ def_id: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/agents`, data),
  getAgentDefinition: (vaultId: string, defId: string) =>
    request<unknown>('GET', `/vaults/${encodeURIComponent(vaultId)}/agents/${encodeURIComponent(defId)}`),
  updateAgentDefinition: (vaultId: string, defId: string, data: unknown) =>
    request<{ ok: boolean }>('PUT', `/vaults/${encodeURIComponent(vaultId)}/agents/${encodeURIComponent(defId)}`, data),
  deleteAgentDefinition: (vaultId: string, defId: string) =>
    request<{ ok: boolean }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/agents/${encodeURIComponent(defId)}`),
  listAgentSkills: (vaultId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/skills`),
  createAgentSkill: (vaultId: string, data: unknown) =>
    request<{ skill_id: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/skills`, data),
  updateAgentSkill: (vaultId: string, skillId: string, data: unknown) =>
    request<{ ok: boolean }>('PUT', `/vaults/${encodeURIComponent(vaultId)}/skills/${encodeURIComponent(skillId)}`, data),
  deleteAgentSkill: (vaultId: string, skillId: string) =>
    request<{ ok: boolean }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/skills/${encodeURIComponent(skillId)}`),
  listAgentTools: () => request<unknown[]>('GET', '/agent-tools'),
  createAgentTool: (data: unknown) => request<{ tool_id: string }>('POST', '/agent-tools', data),

  // Memory
  listMemoryRules: (vaultId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/memory/rules`),
  createMemoryRule: (vaultId: string, data: unknown) =>
    request<{ id: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/memory/rules`, data),
  deleteMemoryRule: (vaultId: string, ruleId: string) =>
    request<{ ok: boolean }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/memory/rules/${encodeURIComponent(ruleId)}`),
  listActivityPatterns: (vaultId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/activity-patterns`),
  upsertActivityPattern: (vaultId: string, data: unknown) =>
    request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/activity-patterns`, data),

  // Scheduled tasks
  listScheduledTasks: (vaultId?: string) =>
    request<unknown[]>('GET', `/scheduled-tasks${vaultId ? `?vault_id=${encodeURIComponent(vaultId)}` : ''}`),
  createScheduledTask: (data: unknown) => request<{ task_id: string }>('POST', '/scheduled-tasks', data),
  deleteScheduledTask: (taskId: string) =>
    request<{ ok: boolean }>('DELETE', `/scheduled-tasks/${encodeURIComponent(taskId)}`),

  // Health
  health: () => fetch('http://127.0.0.1:7787/health').then(r => r.ok),
}
