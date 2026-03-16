import { create } from 'zustand'
import { invoke } from '@tauri-apps/api/core'

export interface SessionInfo {
  token: string
  username: string
  expires_at: number
  auth_provider: string // "local" | "google"
}

interface AuthStore {
  session: SessionInfo | null
  isLoading: boolean
  error: string | null

  checkSession: () => Promise<void>
  login: (username: string, password: string) => Promise<void>
  loginWithGoogle: () => Promise<void>
  logout: () => Promise<void>
  clearError: () => void
}

export const useAuthStore = create<AuthStore>((set) => ({
  session: null,
  isLoading: true,
  error: null,

  checkSession: async () => {
    set({ isLoading: true })
    try {
      const session = await invoke<SessionInfo | null>('get_session')
      set({ session, isLoading: false })
    } catch {
      set({ session: null, isLoading: false })
    }
  },

  login: async (username: string, password: string) => {
    set({ isLoading: true, error: null })
    try {
      const session = await invoke<SessionInfo>('login', { username, password })
      set({ session, isLoading: false })
    } catch (e: any) {
      set({ isLoading: false, error: typeof e === 'string' ? e : '登入失敗' })
      throw e
    }
  },

  loginWithGoogle: async () => {
    set({ isLoading: true, error: null })
    try {
      const session = await invoke<SessionInfo>('start_google_oauth')
      set({ session, isLoading: false })
    } catch (e: any) {
      set({ isLoading: false, error: typeof e === 'string' ? e : 'Google 登入失敗' })
      throw e
    }
  },

  logout: async () => {
    await invoke('logout')
    set({ session: null })
  },

  clearError: () => set({ error: null }),
}))
