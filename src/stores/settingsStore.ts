import { create } from 'zustand'
import { invoke } from '@tauri-apps/api/core'
import { Settings, DEFAULT_SETTINGS } from '../types/settings'

interface SettingsStore {
  settings: Settings
  isLoaded: boolean
  load: () => Promise<void>
  save: (partial: Partial<Settings>) => Promise<void>
  getApiKey: (provider: string) => Promise<string | null>
  setApiKey: (provider: string, key: string) => Promise<void>
}

export const useSettingsStore = create<SettingsStore>((set, get) => ({
  settings: DEFAULT_SETTINGS,
  isLoaded: false,

  load: async () => {
    try {
      const raw = await invoke<any>('get_settings')
      // 合併 DEFAULT_SETTINGS 以補齊新增欄位；sort_orders 由 JSON 字串轉物件
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

  save: async (partial) => {
    const current = get().settings
    const updated = { ...current, ...partial }
    set({ settings: updated })
    try {
      // sort_orders 序列化為 JSON 字串傳給 Rust
      const rustSettings = {
        ...updated,
        sort_orders: JSON.stringify(updated.sort_orders ?? {}),
      }
      await invoke('save_settings', { settings: rustSettings })
    } catch (err) {
      console.error('儲存設定失敗：', err)
      throw err
    }
  },

  getApiKey: async (provider) => {
    try {
      return await invoke<string | null>('get_api_key', { provider })
    } catch {
      return null
    }
  },

  setApiKey: async (provider, key) => {
    await invoke('set_api_key', { provider, key })
  },
}))
