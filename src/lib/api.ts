const BASE = 'http://127.0.0.1:7787/api/v1'

// Auth token management
let _token: string | null = localStorage.getItem('daemon_token')

// Helper: resolve vault UUID from Tauri AppState (daemon-generated)
// invoke is imported lazily to avoid bundler issues
async function getVaultId(): Promise<string> {
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    return await invoke<string>('get_vault_uuid')
  } catch {
    return ''
  }
}

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
  listConversations: (vaultId?: string, mode?: string, accountId?: string) => {
    const params = new URLSearchParams()
    if (vaultId) params.set('vault_id', vaultId)
    if (mode) params.set('mode', mode)
    if (accountId) params.set('account_id', accountId)
    const qs = params.toString()
    return request<unknown[]>('GET', `/conversations${qs ? `?${qs}` : ''}`)
  },
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

  // Auth alias
  getSession: () => request<{ username: string; token: string; expires_at: number; auth_provider: string } | null>('GET', '/auth/session'),
  changePassword: (currentPassword: string, newPassword: string) =>
    request<{ ok: boolean }>('POST', '/auth/change-password', { current_password: currentPassword, new_password: newPassword }),

  // Settings — conversation ID helpers
  getLastChatConversationId: async (vaultPath: string) => {
    try {
      const s = await request<Record<string, string>>('GET', '/settings/user')
      return s[`last_chat_conv_${vaultPath}`] ?? null
    } catch { return null }
  },
  setLastChatConversationId: (vaultPath: string, conversationId: string | null) =>
    request<{ ok: boolean }>('POST', '/settings/user', { [`last_chat_conv_${vaultPath}`]: conversationId ?? '' }),
  getLastModeConversationId: async (vaultPath: string, mode: string) => {
    try {
      const s = await request<Record<string, string>>('GET', '/settings/user')
      return s[`last_mode_conv_${mode}_${vaultPath}`] ?? null
    } catch { return null }
  },
  setLastModeConversationId: (vaultPath: string, mode: string, conversationId: string | null) =>
    request<{ ok: boolean }>('POST', '/settings/user', { [`last_mode_conv_${mode}_${vaultPath}`]: conversationId ?? '' }),

  // API keys
  getApiKey: (provider: string) => request<{ key: string | null }>('GET', `/settings/api-key/${encodeURIComponent(provider)}`),
  setApiKey: (provider: string, key: string) => request<{ ok: boolean }>('POST', `/settings/api-key/${encodeURIComponent(provider)}`, { key }),
  syncBraveKeyId: (keyId: string) => request<{ ok: boolean }>('POST', '/settings/brave-key-id', { key_id: keyId }),

  // Conversations extras
  getOrCreateLiveChatConv: (vaultId: string, accountId: string) => request<{ id: string }>('POST', '/conversations/live-chat', { vault_id: vaultId, account_id: accountId }),
  markConversationProcessed: (id: string) => request<{ ok: boolean }>('PATCH', `/conversations/${encodeURIComponent(id)}/processed`, {}),
  getUnprocessedConversations: (vaultId: string) => request<unknown[]>('GET', `/conversations?vault_id=${encodeURIComponent(vaultId)}&unprocessed=true`),
  getConversationRatings: (conversationId: string) => request<Array<{ content_hash: string; rating: string }>>('GET', `/conversations/${encodeURIComponent(conversationId)}/ratings`),
  rateResponse: (conversationId: string | undefined, contentHash: string, rating: string) =>
    conversationId ? request<{ ok: boolean }>('POST', `/conversations/${encodeURIComponent(conversationId)}/rate`, { content_hash: contentHash, rating }) : Promise.resolve({ ok: false }),
  saveKbChatMessages: (sessionId: string, messages: unknown) =>
    request<{ ok: boolean }>('PUT', `/conversations/kb/${encodeURIComponent(sessionId)}/messages`, { messages }),
  getKbChatMessages: (sessionId: string) =>
    request<string | null>('GET', `/conversations/kb/${encodeURIComponent(sessionId)}/messages`),

  listFolders: (vaultId: string) =>
    request<string[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/folders/list`),
  listAssets: (vaultId: string) =>
    request<string[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/assets/list`),
  trashFolder: async (folderPath: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean; count: number }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/folders/trash?path=${encodeURIComponent(folderPath)}`)
  },
  importAsset: async (filename: string, contentBase64: string, folder?: string | null, newName?: string | null) => {
    const vaultId = await getVaultId()
    return request<{ rel_path: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/assets/import`, {
      filename, content_base64: contentBase64, folder: folder ?? '', new_name: newName ?? '',
    })
  },
  renameAsset: async (path: string, newName: string) => {
    const vaultId = await getVaultId()
    return request<{ new_path: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/assets/rename`, { path, new_name: newName })
  },

  // Notes extras — file operations
  renameNote: async (oldPath: string, newTitle: string) => {
    const vaultId = await getVaultId()
    return request<{ new_path: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/notes/rename`, { old_path: oldPath, new_title: newTitle })
  },
  listTrash: async () => {
    const vaultId = await getVaultId()
    return request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/trash`)
  },
  restoreTrashItem: async (id: string, targetFolder: string) => {
    const vaultId = await getVaultId()
    return request<{ new_path: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/trash/restore`, { id, target_folder: targetFolder })
  },
  createFolder: async (folderPath: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/folders`, { folder_path: folderPath })
  },
  renameFolder: async (folderPath: string, newName: string) => {
    const vaultId = await getVaultId()
    return request<{ new_folder_path: string }>('POST', `/vaults/${encodeURIComponent(vaultId)}/folders/rename`, { folder_path: folderPath, new_name: newName })
  },
  setNoteStatus: async (path: string, status: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('PATCH', `/vaults/${encodeURIComponent(vaultId)}/notes/status`, { path, status })
  },
  trashNote: async (path: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/notes/trash?path=${encodeURIComponent(path)}`)
  },
  deleteTrashItems: async (itemIds: string[]) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/trash`, { item_ids: itemIds })
  },
  deleteAsset: async (path: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/assets?path=${encodeURIComponent(path)}`)
  },

  // Memory
  queryMemory: async (keywords: string[], limit: number) => {
    const vaultId = await getVaultId()
    return request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/memory/query?keywords=${encodeURIComponent(keywords.join(','))}&limit=${limit}`)
  },
  updatePatternScore: (vaultId: string, signature: string, positive: boolean) =>
    request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/activity-patterns/score`, { signature, spoke: positive }),
  decayPatterns: (vaultId: string) =>
    request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/activity-patterns/decay`, {}),
  setPatternIntent: (vaultId: string, signature: string, intent: string) =>
    request<{ ok: boolean }>('PATCH', `/vaults/${encodeURIComponent(vaultId)}/activity-patterns/intent`, { signature, semantic_intent: intent }),

  // KB extras
  queryKb: async (params: { queryId: string; question: string; sessionId?: string }) => {
    const vaultId = await getVaultId()
    const qs = `query_id=${encodeURIComponent(params.queryId)}&q=${encodeURIComponent(params.question)}${params.sessionId ? `&session_id=${encodeURIComponent(params.sessionId)}` : ''}`
    return request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/kb/query?${qs}`)
  },
  queryKnowledge: async (params: { queryId: string; question: string; sessionId?: string }) => {
    const vaultId = await getVaultId()
    const qs = `query_id=${encodeURIComponent(params.queryId)}&q=${encodeURIComponent(params.question)}${params.sessionId ? `&session_id=${encodeURIComponent(params.sessionId)}` : ''}`
    return request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/kb/knowledge?${qs}`)
  },
  deleteKnowledgeItem: async (itemId: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('DELETE', `/vaults/${encodeURIComponent(vaultId)}/kb/items/${encodeURIComponent(itemId)}`)
  },
  renameKnowledgeItem: async (itemId: string, title: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('PATCH', `/vaults/${encodeURIComponent(vaultId)}/kb/items/${encodeURIComponent(itemId)}`, { title })
  },
  fetchSiteOutline: async (sessionId: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/kb/sessions/${encodeURIComponent(sessionId)}/outline`, {})
  },
  importPage: async (sessionId: string, pageId: string) => {
    const vaultId = await getVaultId()
    return request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/kb/sessions/${encodeURIComponent(sessionId)}/pages/${encodeURIComponent(pageId)}/import`, {})
  },
  suggestNoteCards: async (itemId: string) => {
    const vaultId = await getVaultId()
    return request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/kb/items/${encodeURIComponent(itemId)}/suggest-notes`)
  },
  suggestSkillCards: async (itemId: string) => {
    const vaultId = await getVaultId()
    return request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/kb/items/${encodeURIComponent(itemId)}/suggest-skills`)
  },

  // Agents extras
  wakeAgentDefinition: (vaultId: string, defId: string) =>
    request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/agents/${encodeURIComponent(defId)}/wake`, {}),
  addSkillTrigger: (vaultId: string, skillId: string, useAsk: boolean) =>
    request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/skills/${encodeURIComponent(skillId)}/trigger`, { use_ask: useAsk }),
  getSkillUsageStats: (vaultId: string) =>
    request<unknown[]>('GET', `/vaults/${encodeURIComponent(vaultId)}/skills/usage-stats`),

  // Vault state (last open note)
  getVaultLastNote: async (vaultPath: string) => {
    try {
      const s = await request<Record<string, string>>('GET', '/settings/user')
      return s[`vault_last_note_${vaultPath}`] ?? null
    } catch { return null }
  },
  setVaultLastNote: (vaultPath: string, notePath: string) =>
    request<{ ok: boolean }>('POST', '/settings/user', { [`vault_last_note_${vaultPath}`]: notePath }),

  // Mobile pairing
  pairingInfo: () => request<{ tunnel_url: string | null; local_url: string }>('GET', '/pairing-info'),
  generatePairingCode: () => request<{ pairing_code: string; expires_in: number }>('POST', '/auth/generate-pairing-code'),

  // Health
  health: () => fetch('http://127.0.0.1:7787/health').then(r => r.ok),
}
