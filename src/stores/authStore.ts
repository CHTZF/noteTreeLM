import { create } from 'zustand'
import { invoke } from '@tauri-apps/api/core'
import { api, setToken } from '../lib/api'

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
      const raw = await api.getSession()
      const session: SessionInfo | null = raw ? {
        token: raw.token,
        username: raw.username,
        expires_at: raw.expires_at,
        auth_provider: (raw as SessionInfo).auth_provider ?? 'local',
      } : null
      set({ session, isLoading: false })
    } catch {
      set({ session: null, isLoading: false })
    }
  },

  login: async (username: string, password: string) => {
    set({ isLoading: true, error: null })
    try {
      const raw = await api.login(username, password)
      if (raw.token) setToken(raw.token)
      const session: SessionInfo = {
        token: raw.token,
        username: raw.username,
        expires_at: raw.expires_at,
        auth_provider: (raw as unknown as SessionInfo).auth_provider ?? 'local',
      }
      set({ session, isLoading: false })
    } catch (e: any) {
      set({ isLoading: false, error: typeof e === 'string' ? e : '登入失敗' })
      throw e
    }
  },

  loginWithGoogle: async () => {
    set({ isLoading: true, error: null })
    try {
      // Keep as invoke — OAuth flow is native
      const session = await invoke<SessionInfo>('start_google_oauth')
      if (session.token) setToken(session.token)
      set({ session, isLoading: false })
    } catch (e: any) {
      set({ isLoading: false, error: typeof e === 'string' ? e : 'Google 登入失敗' })
      throw e
    }
  },

  logout: async () => {
    await api.logout()
    setToken(null)
    localStorage.removeItem('auth_token')
    set({ session: null })
  },

  clearError: () => set({ error: null }),
}))
