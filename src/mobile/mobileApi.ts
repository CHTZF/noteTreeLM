// Mobile API client — uses configurable base URL + token from localStorage.
// All calls go to the tunnel URL, not hardcoded localhost.

const STORAGE_BASE = 'mobile_base_url'
const STORAGE_TOKEN = 'mobile_token'
const STORAGE_USERNAME = 'mobile_username'
const STORAGE_VAULT_ID = 'mobile_vault_id'

export function getBaseUrl(): string {
  return localStorage.getItem(STORAGE_BASE) ?? ''
}

export function getToken(): string | null {
  return localStorage.getItem(STORAGE_TOKEN)
}

export function saveAuth(baseUrl: string, token: string, username: string) {
  localStorage.setItem(STORAGE_BASE, baseUrl.replace(/\/$/, ''))
  localStorage.setItem(STORAGE_TOKEN, token)
  localStorage.setItem(STORAGE_USERNAME, username)
}

export function clearAuth() {
  localStorage.removeItem(STORAGE_BASE)
  localStorage.removeItem(STORAGE_TOKEN)
  localStorage.removeItem(STORAGE_USERNAME)
  localStorage.removeItem(STORAGE_VAULT_ID)
}

export function getUsername(): string | null {
  return localStorage.getItem(STORAGE_USERNAME)
}

export function getSavedVaultId(): string | null {
  return localStorage.getItem(STORAGE_VAULT_ID)
}

export function saveVaultId(vaultId: string) {
  localStorage.setItem(STORAGE_VAULT_ID, vaultId)
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const base = getBaseUrl()
  if (!base) throw new Error('No base URL configured')
  const token = getToken()
  const headers: Record<string, string> = { 'Content-Type': 'application/json' }
  if (token) headers['Authorization'] = `Bearer ${token}`
  const res = await fetch(`${base}/api/v1${path}`, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  })
  if (!res.ok) {
    const err = await res.text()
    throw new Error(err || `HTTP ${res.status}`)
  }
  return res.json()
}

export const mobileApi = {
  // Pairing
  mobilePair: (baseUrl: string, pairingCode: string, deviceName: string) => {
    const trimmed = baseUrl.replace(/\/$/, '')
    const token = getToken()
    const headers: Record<string, string> = { 'Content-Type': 'application/json' }
    if (token) headers['Authorization'] = `Bearer ${token}`
    return fetch(`${trimmed}/api/v1/auth/mobile-pair`, {
      method: 'POST',
      headers,
      body: JSON.stringify({ pairing_code: pairingCode, device_name: deviceName }),
    }).then(async r => {
      if (!r.ok) throw new Error(await r.text() || `HTTP ${r.status}`)
      return r.json() as Promise<{ token: string; username: string; expires_at: number }>
    })
  },

  // Vaults
  listVaults: () => request<Array<{ vault_id: string; path: string; name?: string }>>('GET', '/vaults'),

  // Conversations
  listConversations: (vaultId: string) =>
    request<Array<{ id: string; title: string; updated_at: number }>>('GET', `/conversations?vault_id=${encodeURIComponent(vaultId)}&mode=chat`),
  getConversation: (id: string) =>
    request<{ id: string; messages_json: string }>('GET', `/conversations/${encodeURIComponent(id)}`),

  // Agent run
  runAgent: (vaultId: string, body: {
    session_id: string
    input: string
    conversation_id?: string
    messages?: unknown[]
    vault_path?: string
  }) => request<{ session_id: string; conversation_id: string }>(
    'POST',
    `/vaults/${encodeURIComponent(vaultId)}/agent/run`,
    body,
  ),

  cancelAgent: (vaultId: string, sessionId: string) =>
    request<{ ok: boolean }>('POST', `/vaults/${encodeURIComponent(vaultId)}/agent/cancel`, { session_id: sessionId }),

  // Settings (for vault path)
  getSettings: () => request<Record<string, string>>('GET', '/settings'),
}

/** Build the SSE URL for the events stream. Returned as a string so the
 *  caller can construct an EventSource with it. */
export function sseUrl(): string {
  const base = getBaseUrl()
  const token = getToken()
  // EventSource cannot set headers; pass token as query param
  return `${base}/api/v1/events${token ? `?token=${encodeURIComponent(token)}` : ''}`
}
