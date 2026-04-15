import { create } from 'zustand'

interface MicStore {
  /** 目前佔用麥克風的 holder ID（每個 component instance 的唯一字串），null 表示空閒 */
  holder: string | null
  /**
   * 嘗試搶占麥克風。
   * 若空閒或已由同一 holder 占用，返回 true 並更新 holder。
   * 若被其他 holder 占用，返回 false。
   */
  claim: (id: string) => boolean
  /** 釋放麥克風（只有當前 holder 才能釋放）。 */
  release: (id: string) => void
}

export const useMicStore = create<MicStore>((set, get) => ({
  holder: null,
  claim: (id) => {
    const { holder } = get()
    if (holder !== null && holder !== id) return false
    set({ holder: id })
    return true
  },
  release: (id) => {
    if (get().holder === id) set({ holder: null })
  },
}))
