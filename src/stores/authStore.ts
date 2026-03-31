import { create } from 'zustand'
import { invoke } from '@tauri-apps/api/core'
import { setToken } from '../lib/api'

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
      // Pass the saved localStorage token to Tauri so it can also update AppState.
      // restore_session validates with the service and stores in AppState if valid.
      const savedToken = localStorage.getItem('daemon_token') ?? ''
      const session = await invoke<SessionInfo | null>('restore_session', { token: savedToken })
      if (session?.token) setToken(session.token)
      set({ session, isLoading: false })
    } catch {
      set({ session: null, isLoading: false })
    }
  },

  login: async (username: string, password: string) => {
    set({ isLoading: true, error: null })
    try {
      const session = await invoke<SessionInfo>('login', { username, password })
      if (session.token) setToken(session.token)
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
    await invoke('logout').catch(() => {})
    setToken(null)
    set({ session: null })
  },

  clearError: () => set({ error: null }),
}))
