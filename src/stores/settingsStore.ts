import { create } from 'zustand'
import { Settings, DEFAULT_SETTINGS } from '../types/settings'
import { api } from '../lib/api'

export const SYSTEM_KEYS = ['system_current_vault_path', 'ai_provider', 'ai_model', 'ai_base_url',
  'ai_enable_topics', 'ai_enable_summary', 'ai_enable_vision',
  'whisper_cli_path', 'whisper_model_path', 'whisper_language', 'whisper_threads', 'whisper_auto_insert',
  'llm_model_path', 'llama_cli_path', 'embedding_model_path'] as const

export type SystemSettingsKey = typeof SYSTEM_KEYS[number]
export type SystemSettings = Pick<Settings, SystemSettingsKey>

interface SettingsStore {
  settings: Settings
  isLoaded: boolean
  systemSettings: SystemSettings | null
  load: () => Promise<void>
  loadSystem: () => Promise<void>
  savePersonal: (partial: Partial<Settings>) => Promise<void>
  saveSystem: (partial: SystemSettings) => Promise<void>
  getApiKey: (provider: string) => Promise<string | null>
  setApiKey: (provider: string, key: string) => Promise<void>
}

export const useSettingsStore = create<SettingsStore>((set, get) => ({
  settings: DEFAULT_SETTINGS,
  isLoaded: false,
  systemSettings: null,

  load: async () => {
    try {
      const raw = await api.getSettings()
      const settings: Settings = {
        ...DEFAULT_SETTINGS,
        ...raw,
        sort_orders: typeof raw.sort_orders === 'string'
          ? JSON.parse(raw.sort_orders || '{}')
          : (raw.sort_orders ?? {}),
      }
      set({ settings, isLoaded: true })
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
      await api.saveSettings(merged)
      set({ systemSettings: merged })
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
