import { create } from 'zustand'
import { Settings, DEFAULT_SETTINGS } from '../types/settings'
import { api } from '../lib/api'

export const SYSTEM_KEYS = ['system_current_vault_path',
  'ai_enable_topics', 'ai_enable_summary', 'ai_enable_vision',
  'whisper_cli_path', 'whisper_model_path', 'whisper_language', 'whisper_threads', 'whisper_auto_insert',
  'llm_model_path', 'llama_cli_path', 'embedding_model_path'] as const

export type SystemSettingsKey = typeof SYSTEM_KEYS[number]
export type SystemSettings = Pick<Settings, SystemSettingsKey>

interface SettingsStore {
  settings: Settings
  isLoaded: boolean
  systemSettings: SystemSettings | null
  currentVaultId: string
  load: () => Promise<void>
  loadSystem: () => Promise<void>
  savePersonal: (partial: Partial<Settings>) => Promise<void>
  saveSystem: (partial: SystemSettings) => Promise<void>
  getApiKey: (provider: string) => Promise<string | null>
  setApiKey: (provider: string, key: string) => Promise<void>
}

/**
 * Coerce a raw Record<string, string> from the DB back into typed Settings.
 * Uses DEFAULT_SETTINGS as the type oracle: if the default is a number/boolean/array/object,
 * parse the string value accordingly.
 */
function coerceSettings(raw: Record<string, unknown>): Settings {
  const result: Record<string, unknown> = { ...DEFAULT_SETTINGS }
  for (const key of Object.keys(DEFAULT_SETTINGS) as (keyof Settings)[]) {
    const rawVal = raw[key]
    if (rawVal === undefined || rawVal === null) continue
    const defaultVal = DEFAULT_SETTINGS[key]
    if (typeof defaultVal === 'boolean') {
      result[key] = rawVal === 'true' || rawVal === true
    } else if (typeof defaultVal === 'number') {
      const n = Number(rawVal)
      result[key] = isNaN(n) ? defaultVal : n
    } else if (Array.isArray(defaultVal)) {
      if (typeof rawVal === 'string') {
        try { result[key] = JSON.parse(rawVal) } catch { result[key] = defaultVal }
      } else {
        result[key] = rawVal
      }
    } else if (typeof defaultVal === 'object' && defaultVal !== null) {
      if (typeof rawVal === 'string') {
        try { result[key] = JSON.parse(rawVal) } catch { result[key] = defaultVal }
      } else {
        result[key] = rawVal
      }
    } else {
      result[key] = rawVal
    }
  }
  return result as unknown as Settings
}

export const useSettingsStore = create<SettingsStore>((set, get) => ({
  settings: DEFAULT_SETTINGS,
  isLoaded: false,
  systemSettings: null,
  currentVaultId: '',

  load: async () => {
    try {
      // Merge global settings (system) + user settings (personal) in that order
      const [globalRaw, userRaw] = await Promise.all([
        api.getSettings().catch(() => ({} as Record<string, string>)),
        api.getUserSettings().catch(() => ({} as Record<string, string>)),
      ])
      const raw = { ...globalRaw, ...userRaw }
      const settings: Settings = coerceSettings(raw)
      set({ settings, isLoaded: true })
      // Fetch vault UUID and keep it reactive in the store
      const { invoke } = await import('@tauri-apps/api/core')
      const vaultId = await invoke<string>('get_vault_uuid').catch(() => '')
      set({ currentVaultId: vaultId })
    } catch (err) {
      console.error('載入設定失敗：', err)
      set({ isLoaded: true })
    }
  },

  loadSystem: async () => {
    try {
      // get_system_settings returns only system keys
      const { invoke } = await import('@tauri-apps/api/core')
      const raw = await invoke<SystemSettings>('get_system_settings')
      set({ systemSettings: raw })
    } catch (err) {
      console.error('載入系統設定失敗：', err)
    }
  },

  savePersonal: async (partial) => {
    const current = get().settings
    const updated = { ...current, ...partial }
    set({ settings: updated })
    try {
      const rustSettings = {
        ...updated,
        sort_orders: JSON.stringify(updated.sort_orders ?? {}),
      }
      await api.saveUserSettings(rustSettings)
    } catch (err) {
      console.error('儲存個人設定失敗：', err)
      throw err
    }
  },

  saveSystem: async (partial) => {
    try {
      // Rust expects the full SystemSettings struct — load current values first if not available
      if (!get().systemSettings) {
        await get().loadSystem()
      }
      const merged = { ...(get().systemSettings ?? {} as SystemSettings), ...partial } as SystemSettings
      const { invoke } = await import('@tauri-apps/api/core')
      // Use Tauri command (not daemon REST directly) so handle_vault_switch runs and AppState.vault_uuid is updated
      await invoke('save_system_settings', { settings: merged })
      set({ systemSettings: merged, settings: { ...get().settings, ...partial } })
      // Refresh reactive vault UUID after vault switch
      if (partial.system_current_vault_path) {
        const vaultId = await invoke<string>('get_vault_uuid').catch(() => '')
        set({ currentVaultId: vaultId })
      }
    } catch (err) {
      console.error('儲存系統設定失敗：', err)
      throw err
    }
  },

  getApiKey: async (provider) => {
    try {
      const result = await api.getApiKey(provider)
      return result.key ?? null
    } catch {
      return null
    }
  },

  setApiKey: async (provider, key) => {
    await api.setApiKey(provider, key)
  },
}))
